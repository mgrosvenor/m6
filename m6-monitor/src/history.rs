//! A size-capped history of every poll, kept in one SQLite file.
//!
//! The digest holds the latest poll only. With `history_file` set, every poll
//! is also written to a SQLite file, so a reader can ask what a node looked
//! like an hour ago or last week. This module is the operator reference for
//! that feature.
//!
//! # Configuration
//!
//! | key (`[monitor]`) | default | rule |
//! |---|---|---|
//! | `history_file` | absent, meaning off | a non-empty path, relative to the site directory |
//! | `history_max_mb` | 256 | 1 to 1048576, one MB is 2^20 bytes |
//!
//! Both are read once at start, so a change takes effect on restart.
//!
//! History is opt-in. With no `history_file`, `/history` and `/crawlers`
//! answer 503, section I of `/check` says "off" and nothing else changes.
//! A bad key, or a file written by a newer m6-monitor, stops the start with
//! exit status 2. Any other failure to open or write the file switches history
//! off without stopping the poller, and adds one Warn on node `monitor`.
//!
//! # What is stored
//!
//! Each poll writes, in one transaction:
//!
//! - one `poll` row: the verdict, and the fault and warning counts;
//! - one `poll_finding` row per finding, so the history says why a poll was
//!   Warn or Fault;
//! - one `health` row per node: reachability, the node's own verdict level,
//!   build, uptime, request, cache and backend error counters, hit and miss
//!   percentiles with their sample counts, the host figures, the tunnel
//!   figures and the log reopen state;
//! - `health_channel` rows (one per channel with traffic) and `health_net`
//!   rows (one per network interface);
//! - once per node per `/traffic` window, one `crawl_window` row with the
//!   window's total, its response status counts (`crawl_status`) and its
//!   crawler sightings (`crawl_sighting`).
//!
//! Counters are stored as the node reports them, which is cumulative since
//! the node started. A reader differences two polls to get a rate. The hit
//! rate is computed on output. A percentile is `NULL` when its sample count
//! is 0, and an absent figure is `null` in the output and never 0.
//!
//! Names of nodes, channels, interfaces and builds sit in lookup tables that
//! are append-only, so an id is never rewritten. A user agent is text a visitor
//! chooses, so its table is not append-only. Pruning deletes the user agents
//! that no remaining sighting names.
//!
//! One node window stores at most 100 crawler sightings, the ones with the
//! most requests, and each user agent is cut to 256 bytes. Agents that are
//! equal after the cut share one sighting. `crawl_window.sightings_dropped`
//! and the `/crawlers` key `sightings_dropped` say how many were left out.
//!
//! ## Crawler cadence
//!
//! `/traffic` is a rolling summary over `window_minutes`, polled every
//! `poll_interval_s`. Storing every poll would repeat each sighting about 120
//! times. A node's window is stored when it has none yet, or when at least
//! `window_minutes` have passed since its last stored one, so the stored
//! windows tile time without overlap. A restart resumes the tiling from the
//! file.
//!
//! # The cap
//!
//! After each write, when the file's used bytes pass `history_max_mb`, the
//! oldest `poll` rows are deleted, 256 at a time, until the file is 5% under
//! the cap. Every child row goes with its poll. There is no time expiry.
//! Freed pages are reused, so the file stops growing near the cap and is never
//! vacuumed. The first row of a poll goes first, even if the clock has moved
//! back, because `poll.id` only increases.
//!
//! # Writing never slows polling
//!
//! The poller builds a batch and hands it to a writer thread through a queue
//! of 8 polls. A full queue drops the poll and counts it, so the poller never
//! waits. A write that fails drops the connection, and the next poll opens it
//! again. The state `behind` lasts until a write succeeds after a drop.
//!
//! # Reading it
//!
//! Both endpoints answer on the service's unix socket only. The monitor has no
//! listener and no token code. Put an m6-http in front of the socket and
//! protect the routes with `require`, as in the admin API example of the user
//! guide:
//!
//! ```toml
//! [[backend]]
//! name    = "monitor"
//! sockets = "<run dir>/m6-monitor.sock"
//!
//! [[route]]
//! path    = "/history"
//! backend = "monitor"
//! require = "role:<role>"
//! cache   = "no-store"
//!
//! [[route]]
//! path    = "/crawlers"
//! backend = "monitor"
//! require = "role:<role>"
//! cache   = "no-store"
//! ```
//!
//! `GET /history?from=<t>&to=<t>[&node=<name>][&cursor=<c>]` and
//! `GET /crawlers` with the same query answer `application/x-ndjson`, one JSON
//! object per line, ordered by time. `from` and `to` are RFC 3339 UTC times
//! and the range is half open, `[from, to)`, at most 31 days. `node` names one
//! node and an unknown name is a 400. A page holds at most the m6-core default
//! (4 MiB and 10,000 rows). When more rows match, the response carries
//! `Link: <?...&cursor=...>; rel="next"`, and the client follows it until a
//! page has none. Pages are bounded because m6-http buffers a backend body whole.
//!
//! A `/history` line has these keys, and an absent figure is `null`:
//!
//! ```text
//! id, at, at_unix_ms, node, level, poll_level, reachable, status, detail,
//! traffic_error, rtt_ms, binary, version, hash, uptime_s, host_uptime_s,
//! requests_total, monitor_requests, cache_hits, cache_misses, hit_rate,
//! backend_errors, hit_samples, hit_p50_ns, hit_p99_ns, miss_samples,
//! miss_p50_ns, miss_p99_ns, load_one, cpus, memory_used, memory_total_bytes,
//! disk_used, disk_total_bytes, thermal_max_c, cert_not_after_unix,
//! cert_expires_in_s, tunnel, log_reopen, net, channels, findings,
//! other_findings
//! ```
//!
//! `level` and `poll_level` are `Ok`, `Warn` or `Fault`. `findings` are the
//! poll's findings about this node and `other_findings` are the poll's
//! findings about the fleet or the monitor. A `/crawlers` line has: `id`, `at`,
//! `at_unix_ms`, `node`, `window_minutes`, `total_requests`, `status` (a map
//! from response code to count), `forged_bot_requests`, `forgers` and
//! `crawlers`, each crawler being `user_agent`, `requests`, `client_ips` and
//! `paths`, and `sightings_dropped`.
//!
//! The reader opens its own read-only connection and reads a whole page inside
//! one read transaction, so a prune that commits meanwhile cannot leave a row
//! without its channels, interfaces or findings. It reads 256 rows at a time,
//! each with its children, and writes them into the page until the page is
//! full or the range ends. A page of 4 MiB usually fills after 1,500 to 4,000
//! rows, so the rest of the range is not read. The file uses a rollback
//! journal, so the writer waits for the transaction to end, up to its busy
//! timeout of 5 seconds. A write that waits longer fails and that poll is not
//! stored.
//!
//! # Caveats
//!
//! Channel counters include the monitor's own polls and header checks (issue
//! #107). `monitor_requests` on each node row says how many.
//!
//! The node's own periodic stats log is not the history. It lives in the
//! node's journal, which is capped, it is a 10 second series with no channels,
//! no reachability, no host figures and no tunnel, and it disappears exactly
//! when a node is down, which is when a history is wanted.
//!
//! To start fresh, stop the service, delete the file and start it again.
//! Migration 1 is permanent: a later release adds columns in migration 2.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use m6_core::monitoring::CrawlerReport;
use m6_core::sqlite::{
    self,
    rusqlite::{self, named_params, params, Connection, OptionalExtension, Transaction},
    Migration, SqliteError,
};
use m6_core::time_range::rfc3339_utc_ms;
use serde::{Serialize, Serializer};
use serde_json::Value;

use crate::digest::{Digest, Finding, Level, NodeDigest};
use crate::poll::NodeReading;

pub const DEFAULT_MAX_MB: u64 = 256;
pub const MAX_MB: u64 = 1_048_576;
const QUEUE_POLLS: usize = 8;
const PRUNE_BATCH: u32 = 256;

const SCHEMA_V1: &str = "
CREATE TABLE node    (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE) STRICT;
CREATE TABLE channel (id INTEGER PRIMARY KEY, label TEXT NOT NULL UNIQUE) STRICT;
CREATE TABLE iface   (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE) STRICT;
CREATE TABLE agent   (id INTEGER PRIMARY KEY, user_agent TEXT NOT NULL UNIQUE) STRICT;
CREATE TABLE build   (id INTEGER PRIMARY KEY, binary TEXT NOT NULL, version TEXT NOT NULL,
                      hash TEXT NOT NULL, UNIQUE (binary, version, hash)) STRICT;
CREATE TABLE poll (id INTEGER PRIMARY KEY AUTOINCREMENT, at_ms INTEGER NOT NULL,
  level INTEGER NOT NULL CHECK (level IN (0,1,2)), faults INTEGER NOT NULL, warns INTEGER NOT NULL) STRICT;
CREATE INDEX poll_at ON poll (at_ms);
CREATE TABLE poll_finding (
  id INTEGER PRIMARY KEY,
  poll_id INTEGER NOT NULL REFERENCES poll(id) ON DELETE CASCADE,
  level INTEGER NOT NULL CHECK (level IN (0,1,2)),
  node TEXT NOT NULL, polled INTEGER NOT NULL CHECK (polled IN (0,1)), text TEXT NOT NULL) STRICT;
CREATE INDEX poll_finding_poll ON poll_finding (poll_id);
CREATE TABLE health (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  poll_id INTEGER NOT NULL REFERENCES poll(id) ON DELETE CASCADE,
  at_ms INTEGER NOT NULL, node_id INTEGER NOT NULL REFERENCES node(id),
  level INTEGER NOT NULL CHECK (level IN (0,1,2)),
  reachable INTEGER NOT NULL CHECK (reachable IN (0,1)), status TEXT NOT NULL,
  detail TEXT, traffic_error TEXT, rtt_ms REAL, build_id INTEGER REFERENCES build(id),
  uptime_s INTEGER, host_uptime_s INTEGER, requests_total INTEGER, monitor_requests INTEGER,
  cache_hits INTEGER, cache_misses INTEGER, backend_errors INTEGER,
  hit_samples INTEGER, hit_p50_ns INTEGER, hit_p99_ns INTEGER,
  miss_samples INTEGER, miss_p50_ns INTEGER, miss_p99_ns INTEGER,
  load_one REAL, cpus INTEGER, memory_used REAL, memory_total_bytes INTEGER,
  disk_used REAL, disk_total_bytes INTEGER, thermal_max_c REAL, cert_not_after_unix INTEGER,
  tunnel_rtt_ms REAL, tunnel_rtt_n INTEGER, tunnel_probe TEXT, tunnel_probe_error TEXT,
  tunnel_handshake TEXT, tunnel_handshake_age_s INTEGER, tunnel_handshake_detail TEXT,
  reopen_state TEXT, reopen_age_s INTEGER, reopen_last_unix INTEGER, reopen_reopens INTEGER,
  reopen_failures INTEGER, reopen_last_failure_age_s INTEGER, reopen_detail TEXT) STRICT;
CREATE INDEX health_poll ON health (poll_id);
CREATE INDEX health_at ON health (at_ms);
CREATE INDEX health_node_at ON health (node_id, at_ms);
CREATE TABLE health_channel (
  health_id INTEGER NOT NULL REFERENCES health(id) ON DELETE CASCADE,
  channel_id INTEGER NOT NULL REFERENCES channel(id),
  requests INTEGER NOT NULL, hits INTEGER NOT NULL, misses INTEGER NOT NULL, backend_errors INTEGER NOT NULL,
  hit_samples INTEGER NOT NULL, hit_p50_ns INTEGER, hit_p99_ns INTEGER,
  miss_samples INTEGER NOT NULL, miss_p50_ns INTEGER, miss_p99_ns INTEGER,
  PRIMARY KEY (health_id, channel_id)) STRICT, WITHOUT ROWID;
CREATE TABLE health_net (
  health_id INTEGER NOT NULL REFERENCES health(id) ON DELETE CASCADE,
  iface_id INTEGER NOT NULL REFERENCES iface(id),
  rx_bytes INTEGER NOT NULL, rx_errs INTEGER NOT NULL, rx_drop INTEGER NOT NULL,
  tx_bytes INTEGER NOT NULL, tx_errs INTEGER NOT NULL, tx_drop INTEGER NOT NULL,
  PRIMARY KEY (health_id, iface_id)) STRICT, WITHOUT ROWID;
CREATE TABLE crawl_window (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  poll_id INTEGER NOT NULL REFERENCES poll(id) ON DELETE CASCADE,
  at_ms INTEGER NOT NULL, node_id INTEGER NOT NULL REFERENCES node(id),
  window_minutes INTEGER NOT NULL, total_requests INTEGER NOT NULL,
  forged_bot_requests INTEGER NOT NULL, forgers TEXT NOT NULL,
  sightings_dropped INTEGER NOT NULL DEFAULT 0) STRICT;
CREATE INDEX crawl_window_poll ON crawl_window (poll_id);
CREATE INDEX crawl_window_at ON crawl_window (at_ms);
CREATE INDEX crawl_window_node_at ON crawl_window (node_id, at_ms);
CREATE TABLE crawl_sighting (
  window_id INTEGER NOT NULL REFERENCES crawl_window(id) ON DELETE CASCADE,
  agent_id INTEGER NOT NULL REFERENCES agent(id),
  requests INTEGER NOT NULL, client_ips TEXT NOT NULL, paths TEXT NOT NULL,
  PRIMARY KEY (window_id, agent_id)) STRICT, WITHOUT ROWID;
CREATE TABLE crawl_status (
  window_id INTEGER NOT NULL REFERENCES crawl_window(id) ON DELETE CASCADE,
  code INTEGER NOT NULL, requests INTEGER NOT NULL,
  PRIMARY KEY (window_id, code)) STRICT, WITHOUT ROWID;
";

const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    sql: SCHEMA_V1,
}];

/// Lookup tables that are never pruned. `agent` is not here: a user agent is
/// text a visitor chooses, so its rows go when no sighting names them.
const LOOKUP_TABLES: [&str; 4] = ["node", "channel", "iface", "build"];

/// The longest user agent stored, in bytes.
const MAX_AGENT_BYTES: usize = 256;
/// The most crawler sightings stored for one node window.
const MAX_SIGHTINGS: usize = 100;

// ── configuration ────────────────────────────────────────────────────────────

/// The `[monitor]` history keys, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub file: PathBuf,
    pub max_bytes: u64,
}

/// Read `history_file` and `history_max_mb`. `None` means history is off.
pub fn config_from(monitor: Option<&Value>, site_dir: &Path) -> anyhow::Result<Option<Config>> {
    let Some(monitor) = monitor else {
        return Ok(None);
    };
    let max_mb = match monitor.get("history_max_mb") {
        None => DEFAULT_MAX_MB,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_MB).contains(n))
            .ok_or_else(|| {
                anyhow::anyhow!("[monitor] history_max_mb must be an integer from 1 to {MAX_MB}")
            })?,
    };
    let file = match monitor.get("history_file") {
        None => return Ok(None),
        Some(v) => match v.as_str() {
            Some(s) if !s.trim().is_empty() => s,
            _ => anyhow::bail!("[monitor] history_file must be a non-empty path"),
        },
    };
    let file = site_dir.join(file);
    Ok(Some(Config {
        file,
        max_bytes: max_mb << 20,
    }))
}

// ── one poll as rows ─────────────────────────────────────────────────────────

fn level_i(l: Level) -> i64 {
    match l {
        Level::Ok => 0,
        Level::Warn => 1,
        Level::Fault => 2,
    }
}

fn level_name(i: i64) -> &'static str {
    match i {
        0 => "Ok",
        1 => "Warn",
        _ => "Fault",
    }
}

fn i(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn iu(v: usize) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

#[derive(Debug, Clone)]
pub struct FindingRow {
    pub level: i64,
    pub node: String,
    pub polled: bool,
    pub text: String,
}

#[derive(Debug, Clone, Default)]
pub struct TunnelRow {
    pub rtt_ms: Option<f64>,
    pub rtt_n: i64,
    pub probe: String,
    pub probe_error: Option<String>,
    pub handshake: String,
    pub handshake_age_s: Option<i64>,
    pub handshake_detail: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ReopenRow {
    pub state: String,
    pub age_s: Option<i64>,
    pub last_unix: Option<i64>,
    pub reopens: Option<i64>,
    pub failures: Option<i64>,
    pub last_failure_age_s: Option<i64>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ChannelRow {
    pub channel: String,
    pub requests: i64,
    pub hits: i64,
    pub misses: i64,
    pub backend_errors: i64,
    pub hit_samples: i64,
    pub hit_p50_ns: Option<i64>,
    pub hit_p99_ns: Option<i64>,
    pub miss_samples: i64,
    pub miss_p50_ns: Option<i64>,
    pub miss_p99_ns: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NetRow {
    pub iface: String,
    pub rx_bytes: i64,
    pub rx_errs: i64,
    pub rx_drop: i64,
    pub tx_bytes: i64,
    pub tx_errs: i64,
    pub tx_drop: i64,
}

#[derive(Debug, Clone)]
pub struct CrawlSnapshot {
    pub window_minutes: i64,
    pub total_requests: i64,
    pub status: Vec<(u16, i64)>,
    pub forged_bot_requests: i64,
    pub forgers: Vec<String>,
    pub crawlers: Vec<CrawlerReport>,
}

/// One node's row in a poll.
#[derive(Debug, Clone, Default)]
pub struct Row {
    pub node: String,
    pub level: i64,
    pub reachable: bool,
    pub status: String,
    pub detail: Option<String>,
    pub traffic_error: Option<String>,
    pub rtt_ms: Option<f64>,
    pub build: Option<(String, String, String)>,
    pub uptime_s: Option<i64>,
    pub host_uptime_s: Option<i64>,
    pub requests_total: Option<i64>,
    pub monitor_requests: Option<i64>,
    pub cache_hits: Option<i64>,
    pub cache_misses: Option<i64>,
    pub backend_errors: Option<i64>,
    pub hit_samples: Option<i64>,
    pub hit_p50_ns: Option<i64>,
    pub hit_p99_ns: Option<i64>,
    pub miss_samples: Option<i64>,
    pub miss_p50_ns: Option<i64>,
    pub miss_p99_ns: Option<i64>,
    pub load_one: Option<f64>,
    pub cpus: Option<i64>,
    pub memory_used: Option<f64>,
    pub memory_total_bytes: Option<i64>,
    pub disk_used: Option<f64>,
    pub disk_total_bytes: Option<i64>,
    pub thermal_max_c: Option<f64>,
    pub cert_not_after_unix: Option<i64>,
    pub tunnel: Option<TunnelRow>,
    pub reopen: ReopenRow,
    pub channels: Vec<ChannelRow>,
    pub net: Vec<NetRow>,
    pub crawl: Option<CrawlSnapshot>,
}

/// Everything one poll writes.
#[derive(Debug, Clone)]
pub struct Batch {
    pub at_ms: i64,
    pub level: i64,
    pub faults: i64,
    pub warns: i64,
    pub findings: Vec<FindingRow>,
    pub rows: Vec<Row>,
}

fn row(r: &NodeReading, n: &NodeDigest, d: &Digest) -> Row {
    debug_assert_eq!(r.name, n.name);
    let level = d
        .findings
        .iter()
        .filter(|f| f.node == n.name)
        .map(|f| f.level)
        .max()
        .unwrap_or(Level::Ok);
    let present = |s: &Option<String>| s.clone().unwrap_or_default();
    let build = (n.binary.is_some() || n.version.is_some() || n.hash.is_some())
        .then(|| (present(&n.binary), present(&n.version), present(&n.hash)));
    let mut out = Row {
        node: n.name.clone(),
        level: level_i(level),
        reachable: r.is_up(),
        status: n.status.clone(),
        detail: r.unreachable.clone(),
        traffic_error: r.traffic_error.clone(),
        rtt_ms: n.rtt_ms,
        build,
        uptime_s: n.uptime_s.map(i),
        host_uptime_s: n.host_uptime_s.map(i),
        requests_total: n.requests_total.map(i),
        backend_errors: n.backend_errors.map(i),
        hit_samples: n.hit_samples.map(iu),
        hit_p50_ns: n.hit_p50_ns.map(i),
        hit_p99_ns: n.hit_p99_ns.map(i),
        load_one: n.load_one,
        cpus: n.cpus.map(iu),
        memory_used: n.memory_used,
        memory_total_bytes: n.memory_total_bytes.map(i),
        disk_used: n.disk_used,
        disk_total_bytes: n.disk_total_bytes.map(i),
        thermal_max_c: n.thermal_max_c.and_then(|t| t.to_string().parse().ok()),
        cert_not_after_unix: n.cert_not_after_unix,
        tunnel: n.tunnel.as_ref().map(|t| TunnelRow {
            rtt_ms: t.rtt_ms,
            rtt_n: iu(t.rtt_samples_ms.len()),
            probe: t.probe.as_str().to_string(),
            probe_error: t.probe_error.clone(),
            handshake: t.handshake.as_str().to_string(),
            handshake_age_s: t.handshake_age_s.map(i),
            handshake_detail: t.handshake_detail.clone(),
        }),
        reopen: ReopenRow {
            state: n.log_reopen.state.to_string(),
            age_s: n.log_reopen.age_s.map(i),
            last_unix: n.log_reopen.last_unix.map(i),
            reopens: n.log_reopen.reopens.map(i),
            failures: n.log_reopen.failures.map(i),
            last_failure_age_s: n.log_reopen.last_failure_age_s.map(i),
            detail: n.log_reopen.detail.clone(),
        },
        ..Row::default()
    };
    if let Some(p) = &r.perf {
        let m = &p.metrics;
        out.monitor_requests = Some(i(m.monitor_requests_total));
        out.cache_hits = Some(i(m.cache_hits_total));
        out.cache_misses = Some(i(m.cache_misses_total));
        out.miss_samples = Some(iu(m.miss_samples));
        let sampled = m.miss_samples > 0;
        out.miss_p50_ns = sampled.then_some(i(m.miss_p50_ns));
        out.miss_p99_ns = sampled.then_some(i(m.miss_p99_ns));
        out.channels = m
            .channels
            .iter()
            .map(|c| ChannelRow {
                channel: c.channel.clone(),
                requests: i(c.requests),
                hits: i(c.hits),
                misses: i(c.misses),
                backend_errors: i(c.backend_errors),
                hit_samples: iu(c.hit_samples),
                hit_p50_ns: (c.hit_samples > 0).then_some(i(c.hit_p50_ns)),
                hit_p99_ns: (c.hit_samples > 0).then_some(i(c.hit_p99_ns)),
                miss_samples: iu(c.miss_samples),
                miss_p50_ns: (c.miss_samples > 0).then_some(i(c.miss_p50_ns)),
                miss_p99_ns: (c.miss_samples > 0).then_some(i(c.miss_p99_ns)),
            })
            .collect();
        out.net = p
            .host
            .net
            .iter()
            .map(|d| NetRow {
                iface: d.name.clone(),
                rx_bytes: i(d.rx_bytes),
                rx_errs: i(d.rx_errs),
                rx_drop: i(d.rx_drop),
                tx_bytes: i(d.tx_bytes),
                tx_errs: i(d.tx_errs),
                tx_drop: i(d.tx_drop),
            })
            .collect();
    }
    out.crawl = r.traffic.as_ref().map(|t| CrawlSnapshot {
        window_minutes: i(t.window_minutes),
        total_requests: i(t.total_requests),
        status: t.status.iter().map(|(c, n)| (*c, i(*n))).collect(),
        forged_bot_requests: i(t.forged_bot_requests),
        forgers: t.forgers.clone(),
        crawlers: t.crawlers.clone(),
    });
    out
}

/// Turn one poll into rows. Pure: it reads the readings and the digest only.
///
/// Call it after every finding is in the digest, so the stored verdict equals
/// what `/check` showed.
pub fn batch(at_ms: i64, readings: &[NodeReading], d: &Digest) -> Batch {
    debug_assert_eq!(readings.len(), d.nodes.len());
    let faults = d
        .findings
        .iter()
        .filter(|f| f.level == Level::Fault)
        .count();
    let warns = d.findings.iter().filter(|f| f.level == Level::Warn).count();
    Batch {
        at_ms,
        level: level_i(d.level),
        faults: iu(faults),
        warns: iu(warns),
        findings: d
            .findings
            .iter()
            .map(|f| FindingRow {
                level: level_i(f.level),
                node: f.node.clone(),
                polled: d.nodes.iter().any(|n| n.name == f.node),
                text: f.text.clone(),
            })
            .collect(),
        rows: readings
            .iter()
            .zip(&d.nodes)
            .map(|(r, n)| row(r, n, d))
            .collect(),
    }
}

// ── the store ────────────────────────────────────────────────────────────────

/// A lookup table with a cache of the ids it has handed out.
struct Interner {
    insert: &'static str,
    select: &'static str,
    ids: HashMap<String, i64>,
}

impl Interner {
    fn new(insert: &'static str, select: &'static str) -> Self {
        Self {
            insert,
            select,
            ids: HashMap::new(),
        }
    }

    fn id(&mut self, tx: &Transaction, parts: &[&str]) -> rusqlite::Result<i64> {
        let key = parts.join("\u{1f}");
        if let Some(id) = self.ids.get(&key) {
            return Ok(*id);
        }
        tx.prepare_cached(self.insert)?
            .execute(rusqlite::params_from_iter(parts))?;
        let id: i64 = tx
            .prepare_cached(self.select)?
            .query_row(rusqlite::params_from_iter(parts), |r| r.get(0))?;
        self.ids.insert(key, id);
        Ok(id)
    }
}

struct Store {
    conn: Connection,
    max_bytes: u64,
    path: PathBuf,
    nodes: Interner,
    channels: Interner,
    ifaces: Interner,
    agents: Interner,
    builds: Interner,
    last_crawl_at: HashMap<i64, i64>,
}

/// What one write did, for the status.
#[derive(Debug, Default)]
struct Written {
    crawl_windows: u64,
    pruned: u64,
    cap_unmet: bool,
    used_bytes: u64,
    file_bytes: u64,
    oldest_ms: Option<i64>,
    newest_ms: Option<i64>,
}

/// True when the error means the process must not go on: a file from a newer
/// release, or a migration list that is not in order.
fn is_fatal(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<SqliteError>(),
        Some(SqliteError::NewerThanCode { .. } | SqliteError::MigrationList { .. })
    )
}

impl Store {
    fn open(cfg: &Config) -> anyhow::Result<Self> {
        if let Some(dir) = cfg.file.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .map_err(|e| anyhow::anyhow!("creating {}: {e}", dir.display()))?;
        }
        let mut conn = sqlite::open(&cfg.file)?;
        sqlite::migrate(&mut conn, MIGRATIONS)?;
        for t in LOOKUP_TABLES {
            sqlite::append_only(&conn, t)?;
        }
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS agent_append_only_update; \
             DROP TRIGGER IF EXISTS agent_append_only_delete;",
        )?;
        let mut last_crawl_at = HashMap::new();
        {
            let mut stmt =
                conn.prepare("SELECT node_id, max(at_ms) FROM crawl_window GROUP BY node_id")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
            for row in rows {
                let (n, at) = row?;
                last_crawl_at.insert(n, at);
            }
        }
        Ok(Self {
            conn,
            max_bytes: cfg.max_bytes,
            path: cfg.file.clone(),
            nodes: Interner::new(
                "INSERT INTO node (name) VALUES (?1) ON CONFLICT DO NOTHING",
                "SELECT id FROM node WHERE name = ?1",
            ),
            channels: Interner::new(
                "INSERT INTO channel (label) VALUES (?1) ON CONFLICT DO NOTHING",
                "SELECT id FROM channel WHERE label = ?1",
            ),
            ifaces: Interner::new(
                "INSERT INTO iface (name) VALUES (?1) ON CONFLICT DO NOTHING",
                "SELECT id FROM iface WHERE name = ?1",
            ),
            agents: Interner::new(
                "INSERT INTO agent (user_agent) VALUES (?1) ON CONFLICT DO NOTHING",
                "SELECT id FROM agent WHERE user_agent = ?1",
            ),
            builds: Interner::new(
                "INSERT INTO build (binary, version, hash) VALUES (?1, ?2, ?3) ON CONFLICT DO NOTHING",
                "SELECT id FROM build WHERE binary = ?1 AND version = ?2 AND hash = ?3",
            ),
            last_crawl_at,
        })
    }

    fn clear_caches(&mut self) {
        for c in [
            &mut self.nodes,
            &mut self.channels,
            &mut self.ifaces,
            &mut self.agents,
            &mut self.builds,
        ] {
            c.ids.clear();
        }
    }

    /// Write one poll in one transaction, then prune. On error the caller drops
    /// this store, so a rolled-back id is never reused from a cache.
    fn write(&mut self, b: &Batch) -> anyhow::Result<Written> {
        let r = self.insert(b);
        if r.is_err() {
            self.clear_caches();
        }
        let mut w = r?;
        self.prune(&mut w)?;
        Ok(w)
    }

    fn insert(&mut self, b: &Batch) -> anyhow::Result<Written> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut w = Written::default();
        let mut crawled: Vec<(i64, i64)> = Vec::new();
        tx.prepare_cached(
            "INSERT INTO poll (at_ms, level, faults, warns) VALUES (?1, ?2, ?3, ?4)",
        )?
        .execute(params![b.at_ms, b.level, b.faults, b.warns])?;
        let poll_id = tx.last_insert_rowid();
        for f in &b.findings {
            tx.prepare_cached(
                "INSERT INTO poll_finding (poll_id, level, node, polled, text) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![poll_id, f.level, f.node, f.polled, f.text])?;
        }
        for r in &b.rows {
            let node_id = self.nodes.id(&tx, &[&r.node])?;
            let build_id = match &r.build {
                Some((bin, ver, hash)) => Some(self.builds.id(&tx, &[bin, ver, hash])?),
                None => None,
            };
            let t = r.tunnel.as_ref();
            tx.prepare_cached(INSERT_HEALTH)?.execute(named_params! {
                ":poll_id": poll_id, ":at_ms": b.at_ms, ":node_id": node_id, ":level": r.level,
                ":reachable": r.reachable, ":status": r.status, ":detail": r.detail,
                ":traffic_error": r.traffic_error, ":rtt_ms": r.rtt_ms, ":build_id": build_id,
                ":uptime_s": r.uptime_s, ":host_uptime_s": r.host_uptime_s,
                ":requests_total": r.requests_total, ":monitor_requests": r.monitor_requests,
                ":cache_hits": r.cache_hits, ":cache_misses": r.cache_misses,
                ":backend_errors": r.backend_errors,
                ":hit_samples": r.hit_samples, ":hit_p50_ns": r.hit_p50_ns, ":hit_p99_ns": r.hit_p99_ns,
                ":miss_samples": r.miss_samples, ":miss_p50_ns": r.miss_p50_ns, ":miss_p99_ns": r.miss_p99_ns,
                ":load_one": r.load_one, ":cpus": r.cpus, ":memory_used": r.memory_used,
                ":memory_total_bytes": r.memory_total_bytes, ":disk_used": r.disk_used,
                ":disk_total_bytes": r.disk_total_bytes, ":thermal_max_c": r.thermal_max_c,
                ":cert_not_after_unix": r.cert_not_after_unix,
                ":tunnel_rtt_ms": t.and_then(|t| t.rtt_ms), ":tunnel_rtt_n": t.map(|t| t.rtt_n),
                ":tunnel_probe": t.map(|t| t.probe.as_str()),
                ":tunnel_probe_error": t.and_then(|t| t.probe_error.as_deref()),
                ":tunnel_handshake": t.map(|t| t.handshake.as_str()),
                ":tunnel_handshake_age_s": t.and_then(|t| t.handshake_age_s),
                ":tunnel_handshake_detail": t.and_then(|t| t.handshake_detail.as_deref()),
                ":reopen_state": r.reopen.state, ":reopen_age_s": r.reopen.age_s,
                ":reopen_last_unix": r.reopen.last_unix, ":reopen_reopens": r.reopen.reopens,
                ":reopen_failures": r.reopen.failures,
                ":reopen_last_failure_age_s": r.reopen.last_failure_age_s,
                ":reopen_detail": r.reopen.detail,
            })?;
            let health_id = tx.last_insert_rowid();
            for c in &r.channels {
                let channel_id = self.channels.id(&tx, &[&c.channel])?;
                tx.prepare_cached(
                    "INSERT INTO health_channel (health_id, channel_id, requests, hits, misses, \
                     backend_errors, hit_samples, hit_p50_ns, hit_p99_ns, miss_samples, \
                     miss_p50_ns, miss_p99_ns) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                )?
                .execute(params![
                    health_id,
                    channel_id,
                    c.requests,
                    c.hits,
                    c.misses,
                    c.backend_errors,
                    c.hit_samples,
                    c.hit_p50_ns,
                    c.hit_p99_ns,
                    c.miss_samples,
                    c.miss_p50_ns,
                    c.miss_p99_ns
                ])?;
            }
            for n in &r.net {
                let iface_id = self.ifaces.id(&tx, &[&n.iface])?;
                tx.prepare_cached(
                    "INSERT INTO health_net (health_id, iface_id, rx_bytes, rx_errs, rx_drop, \
                     tx_bytes, tx_errs, tx_drop) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                )?
                .execute(params![
                    health_id, iface_id, n.rx_bytes, n.rx_errs, n.rx_drop, n.tx_bytes, n.tx_errs,
                    n.tx_drop
                ])?;
            }
            if let Some(c) = &r.crawl {
                let due = match self.last_crawl_at.get(&node_id) {
                    None => true,
                    Some(last) => {
                        b.at_ms.saturating_sub(*last) >= c.window_minutes.saturating_mul(60_000)
                    }
                };
                if due {
                    insert_window(&mut self.agents, &tx, poll_id, b.at_ms, node_id, c)?;
                    crawled.push((node_id, b.at_ms));
                    w.crawl_windows += 1;
                }
            }
        }
        tx.commit()?;
        // A user agent is not kept for the next poll: most never come back.
        self.agents.ids.clear();
        for (n, at) in crawled {
            self.last_crawl_at.insert(n, at);
        }
        Ok(w)
    }

    /// Delete the oldest polls when the file's used bytes pass the cap, and
    /// with each batch the user agents no sighting names any more.
    fn prune(&mut self, w: &mut Written) -> anyhow::Result<()> {
        let mut used = sqlite::used_bytes(&self.conn)?;
        if used > self.max_bytes {
            let target = self.max_bytes - self.max_bytes / 20;
            while used > target {
                let tx = self
                    .conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let n = tx.execute(
                    "DELETE FROM poll WHERE id IN (SELECT id FROM poll ORDER BY id LIMIT ?1)",
                    [PRUNE_BATCH],
                )?;
                tx.execute(
                    "DELETE FROM agent WHERE id NOT IN (SELECT agent_id FROM crawl_sighting)",
                    [],
                )?;
                tx.commit()?;
                if n == 0 {
                    break;
                }
                w.pruned += n as u64;
                used = sqlite::used_bytes(&self.conn)?;
            }
            w.cap_unmet = used > target;
        }
        w.used_bytes = used;
        w.file_bytes = std::fs::metadata(&self.path).map_or(0, |m| m.len());
        let (oldest, newest): (Option<i64>, Option<i64>) =
            self.conn
                .query_row("SELECT min(at_ms), max(at_ms) FROM poll", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;
        w.oldest_ms = oldest;
        w.newest_ms = newest;
        Ok(())
    }
}

/// A crawler sighting as stored: the agent cut to [`MAX_AGENT_BYTES`].
struct Capped<'a> {
    user_agent: &'a str,
    requests: i64,
    client_ips: Cow<'a, [String]>,
    paths: Cow<'a, [String]>,
}

/// The longest prefix of `s` of at most `max` bytes that ends on a character.
fn truncate_utf8(s: &str, max: usize) -> &str {
    let mut n = max.min(s.len());
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

fn merge_list(into: &mut Cow<'_, [String]>, more: &[String]) {
    let v = into.to_mut();
    for s in more {
        if !v.contains(s) {
            v.push(s.clone());
        }
    }
}

/// The sightings a window stores, busiest first, and how many were left out.
///
/// Agents that are equal once cut share one sighting, with their requests
/// added. At most [`MAX_SIGHTINGS`] stay. Ties go to the lower agent text, so
/// the choice is the same on every run.
fn cap_sightings(crawlers: &[CrawlerReport]) -> (Vec<Capped<'_>>, i64) {
    let mut out: Vec<Capped> = Vec::with_capacity(crawlers.len().min(MAX_SIGHTINGS * 2));
    let mut at: HashMap<&str, usize> = HashMap::with_capacity(crawlers.len());
    for c in crawlers {
        let ua = truncate_utf8(&c.user_agent, MAX_AGENT_BYTES);
        if let Some(&k) = at.get(ua) {
            let m = &mut out[k];
            m.requests = m.requests.saturating_add(i(c.requests));
            merge_list(&mut m.client_ips, &c.client_ips);
            merge_list(&mut m.paths, &c.paths);
        } else {
            at.insert(ua, out.len());
            out.push(Capped {
                user_agent: ua,
                requests: i(c.requests),
                client_ips: Cow::Borrowed(&c.client_ips),
                paths: Cow::Borrowed(&c.paths),
            });
        }
    }
    out.sort_by(|a, b| {
        b.requests
            .cmp(&a.requests)
            .then_with(|| a.user_agent.cmp(b.user_agent))
    });
    let dropped = out.len().saturating_sub(MAX_SIGHTINGS);
    out.truncate(MAX_SIGHTINGS);
    (out, iu(dropped))
}

fn insert_window(
    agents: &mut Interner,
    tx: &Transaction,
    poll_id: i64,
    at_ms: i64,
    node_id: i64,
    c: &CrawlSnapshot,
) -> anyhow::Result<()> {
    let (sightings, dropped) = cap_sightings(&c.crawlers);
    tx.prepare_cached(
        "INSERT INTO crawl_window (poll_id, at_ms, node_id, window_minutes, total_requests, \
         forged_bot_requests, forgers, sightings_dropped) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
    )?
    .execute(params![
        poll_id,
        at_ms,
        node_id,
        c.window_minutes,
        c.total_requests,
        c.forged_bot_requests,
        serde_json::to_string(&c.forgers)?,
        dropped
    ])?;
    let window_id = tx.last_insert_rowid();
    for (code, n) in &c.status {
        tx.prepare_cached(
            "INSERT INTO crawl_status (window_id, code, requests) VALUES (?1, ?2, ?3)",
        )?
        .execute(params![window_id, code, n])?;
    }
    for s in &sightings {
        let agent_id = agents.id(tx, &[s.user_agent])?;
        tx.prepare_cached(
            "INSERT INTO crawl_sighting (window_id, agent_id, requests, client_ips, paths) \
             VALUES (?1,?2,?3,?4,?5)",
        )?
        .execute(params![
            window_id,
            agent_id,
            s.requests,
            serde_json::to_string(&s.client_ips)?,
            serde_json::to_string(&s.paths)?
        ])?;
    }
    Ok(())
}

const INSERT_HEALTH: &str = "INSERT INTO health (poll_id, at_ms, node_id, level, reachable, status, \
detail, traffic_error, rtt_ms, build_id, uptime_s, host_uptime_s, requests_total, monitor_requests, \
cache_hits, cache_misses, backend_errors, hit_samples, hit_p50_ns, hit_p99_ns, miss_samples, \
miss_p50_ns, miss_p99_ns, load_one, cpus, memory_used, memory_total_bytes, disk_used, \
disk_total_bytes, thermal_max_c, cert_not_after_unix, tunnel_rtt_ms, tunnel_rtt_n, tunnel_probe, \
tunnel_probe_error, tunnel_handshake, tunnel_handshake_age_s, tunnel_handshake_detail, \
reopen_state, reopen_age_s, reopen_last_unix, reopen_reopens, reopen_failures, \
reopen_last_failure_age_s, reopen_detail) VALUES (:poll_id, :at_ms, :node_id, :level, :reachable, \
:status, :detail, :traffic_error, :rtt_ms, :build_id, :uptime_s, :host_uptime_s, :requests_total, \
:monitor_requests, :cache_hits, :cache_misses, :backend_errors, :hit_samples, :hit_p50_ns, \
:hit_p99_ns, :miss_samples, :miss_p50_ns, :miss_p99_ns, :load_one, :cpus, :memory_used, \
:memory_total_bytes, :disk_used, :disk_total_bytes, :thermal_max_c, :cert_not_after_unix, \
:tunnel_rtt_ms, :tunnel_rtt_n, :tunnel_probe, :tunnel_probe_error, :tunnel_handshake, \
:tunnel_handshake_age_s, :tunnel_handshake_detail, :reopen_state, :reopen_age_s, :reopen_last_unix, \
:reopen_reopens, :reopen_failures, :reopen_last_failure_age_s, :reopen_detail)";

// ── status ───────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Status {
    open_error: Option<String>,
    last_error: Option<String>,
    writes: u64,
    crawl_windows: u64,
    failures: u64,
    pruned: u64,
    dropped_acked: u64,
    last_write: Option<Instant>,
    used_bytes: Option<u64>,
    file_bytes: Option<u64>,
    oldest_ms: Option<i64>,
    newest_ms: Option<i64>,
}

struct Shared {
    cfg: Option<Config>,
    status: Mutex<Status>,
    dropped: AtomicU64,
    stopped: AtomicBool,
}

impl Shared {
    fn status(&self) -> std::sync::MutexGuard<'_, Status> {
        self.status.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The history as the digest reports it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct HistoryDigest {
    /// `off`, `ok`, `not_open`, `failing`, `behind` or `stopped`.
    pub state: &'static str,
    pub file: Option<String>,
    pub max_bytes: Option<u64>,
    pub file_bytes: Option<u64>,
    pub used_bytes: Option<u64>,
    pub oldest_at: Option<String>,
    pub newest_at: Option<String>,
    pub last_write_age_s: Option<u64>,
    pub writes: u64,
    pub crawl_windows: u64,
    pub failures: u64,
    pub dropped: u64,
    pub pruned: u64,
    pub last_error: Option<String>,
}

/// Read side of the status, and the path for the endpoints.
#[derive(Clone)]
pub struct HistoryHandle(Arc<Shared>);

impl HistoryHandle {
    pub fn file(&self) -> Option<&Path> {
        self.0.cfg.as_ref().map(|c| c.file.as_path())
    }

    pub fn digest(&self) -> HistoryDigest {
        let s = self.0.status();
        let dropped = self.0.dropped.load(Ordering::Relaxed);
        let state = if self.0.cfg.is_none() {
            "off"
        } else if self.0.stopped.load(Ordering::Relaxed) {
            "stopped"
        } else if s.open_error.is_some() {
            "not_open"
        } else if s.last_error.is_some() {
            "failing"
        } else if dropped > s.dropped_acked {
            "behind"
        } else {
            "ok"
        };
        let at = |ms: Option<i64>| ms.map(|m| rfc3339_utc_ms(m).to_string());
        HistoryDigest {
            state,
            file: self.0.cfg.as_ref().map(|c| c.file.display().to_string()),
            max_bytes: self.0.cfg.as_ref().map(|c| c.max_bytes),
            file_bytes: s.file_bytes,
            used_bytes: s.used_bytes,
            oldest_at: at(s.oldest_ms),
            newest_at: at(s.newest_ms),
            last_write_age_s: s.last_write.map(|t| t.elapsed().as_secs()),
            writes: s.writes,
            crawl_windows: s.crawl_windows,
            failures: s.failures,
            dropped,
            pruned: s.pruned,
            last_error: s.open_error.clone().or_else(|| s.last_error.clone()),
        }
    }
}

/// The Warn that goes on node `monitor` when history is not working.
pub fn finding(h: &HistoryDigest) -> Option<Finding> {
    let err = h.last_error.as_deref().unwrap_or("-");
    let text = match h.state {
        "not_open" => format!(
            "history file {} could not be opened, so no history is being kept: {err}",
            h.file.as_deref().unwrap_or("-")
        ),
        "failing" => format!(
            "the last history write failed, so that poll is missing from the history: {err}"
        ),
        "behind" => format!(
            "the history writer is behind and has dropped {} poll(s) since start: \
             is the disk slow or the file locked?",
            h.dropped
        ),
        "stopped" => "the history writer has stopped, so no history is being kept".to_string(),
        _ => return None,
    };
    Some(Finding {
        level: Level::Warn,
        node: "monitor".to_string(),
        text,
    })
}

/// The poller's side. `record` never blocks.
pub struct Recorder {
    tx: Option<SyncSender<Batch>>,
    shared: Arc<Shared>,
}

impl Recorder {
    pub fn record(&self, b: Batch) {
        let Some(tx) = &self.tx else { return };
        match tx.try_send(b) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {
                self.shared.stopped.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// Applies batches to the store and keeps the status.
struct Writer {
    store: Option<Store>,
    cfg: Config,
    shared: Arc<Shared>,
}

impl Writer {
    fn handle(&mut self, b: &Batch) {
        if self.store.is_none() {
            match Store::open(&self.cfg) {
                Ok(s) => {
                    self.store = Some(s);
                    let mut st = self.shared.status();
                    if st.open_error.take().is_some() {
                        tracing::info!(file = %self.cfg.file.display(), "history file opened");
                    }
                }
                Err(e) => {
                    self.fail(format!("{e:#}"), true);
                    return;
                }
            }
        }
        let Some(store) = self.store.as_mut() else {
            return;
        };
        match store.write(b) {
            Ok(w) => {
                let mut st = self.shared.status();
                if !w.cap_unmet && st.last_error.take().is_some() {
                    tracing::info!("history writes recovered");
                }
                st.writes += 1;
                st.crawl_windows += w.crawl_windows;
                st.pruned += w.pruned;
                st.last_write = Some(Instant::now());
                st.used_bytes = Some(w.used_bytes);
                st.file_bytes = Some(w.file_bytes);
                st.oldest_ms = w.oldest_ms;
                st.newest_ms = w.newest_ms;
                st.dropped_acked = self.shared.dropped.load(Ordering::Relaxed);
                if w.cap_unmet {
                    drop(st);
                    self.fail(
                        "the history is over its cap and pruning every poll cannot get under it"
                            .to_string(),
                        false,
                    );
                }
            }
            Err(e) => {
                self.store = None;
                self.fail(format!("{e:#}"), false);
            }
        }
    }

    fn fail(&self, msg: String, open: bool) {
        let mut st = self.shared.status();
        st.failures += 1;
        let slot = if open {
            &mut st.open_error
        } else {
            &mut st.last_error
        };
        if slot.as_deref() != Some(msg.as_str()) {
            tracing::error!(error = %msg, "history is not being written");
        }
        *slot = Some(msg);
    }
}

/// Marks the history stopped when the writer thread ends for any reason.
struct StopGuard(Arc<Shared>);

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.0.stopped.store(true, Ordering::Relaxed);
    }
}

fn new_shared(cfg: Option<Config>) -> Arc<Shared> {
    Arc::new(Shared {
        cfg,
        status: Mutex::new(Status::default()),
        dropped: AtomicU64::new(0),
        stopped: AtomicBool::new(false),
    })
}

/// Open the file and start the writer thread.
///
/// A fatal error (a file from a newer release, a bad migration list) is `Err`
/// and the service exits 2. Any other failure starts the writer with no file
/// and state `not_open`, and the next poll tries again.
pub fn start(cfg: Option<Config>) -> anyhow::Result<(Recorder, HistoryHandle)> {
    let shared = new_shared(cfg.clone());
    let Some(cfg) = cfg else {
        return Ok((
            Recorder {
                tx: None,
                shared: Arc::clone(&shared),
            },
            HistoryHandle(shared),
        ));
    };
    let store = match Store::open(&cfg) {
        Ok(s) => Some(s),
        Err(e) if is_fatal(&e) => {
            return Err(e.context(format!("history file {}", cfg.file.display())))
        }
        Err(e) => {
            let msg = format!("{e:#}");
            tracing::error!(error = %msg, "history is not being written");
            shared.status().open_error = Some(msg);
            None
        }
    };
    let (tx, rx) = sync_channel(QUEUE_POLLS);
    spawn_writer(
        rx,
        Writer {
            store,
            cfg,
            shared: Arc::clone(&shared),
        },
    )?;
    Ok((
        Recorder {
            tx: Some(tx),
            shared: Arc::clone(&shared),
        },
        HistoryHandle(shared),
    ))
}

fn spawn_writer(rx: Receiver<Batch>, mut w: Writer) -> anyhow::Result<()> {
    let guard = StopGuard(Arc::clone(&w.shared));
    std::thread::Builder::new()
        .name("m6-monitor-history".into())
        .spawn(move || {
            let _guard = guard;
            for b in rx {
                w.handle(&b);
            }
        })?;
    Ok(())
}

// ── reading ──────────────────────────────────────────────────────────────────

/// The one `node` value of a query string, url-decoded.
pub fn node_param(query: &str) -> Result<Option<String>, &'static str> {
    match m6_core::request::single_query_value(query, "node") {
        Err(m6_core::request::RepeatedKey) => Err("`node` given more than once"),
        Ok(Some("")) => Err("`node` is empty"),
        Ok(v) => Ok(v.map(m6_core::request::url_decode)),
    }
}

/// The id of a node name, `None` when the history has never seen it.
pub fn node_id(conn: &Connection, name: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row("SELECT id FROM node WHERE name = ?1", [name], |r| r.get(0))
        .optional()
}

/// Where a page starts: the `(at_unix_ms, id)` of its first row.
pub fn parse_cursor(s: &str) -> Option<(i64, i64)> {
    let (a, b) = s.split_once('-')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

pub fn cursor_text(at_ms: i64, id: i64) -> String {
    format!("{at_ms}-{id}")
}

fn ser_at<S: Serializer>(ms: &i64, s: S) -> Result<S::Ok, S::Error> {
    s.collect_str(&rfc3339_utc_ms(*ms))
}

fn ser_level<S: Serializer>(l: &i64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(level_name(*l))
}

fn rate(hits: Option<i64>, misses: Option<i64>) -> Option<f64> {
    let (h, m) = (hits?, misses?);
    (h + m > 0).then(|| h as f64 / (h + m) as f64)
}

#[derive(Debug, Serialize)]
pub struct TunnelOut {
    pub rtt_ms: Option<f64>,
    pub rtt_n: Option<i64>,
    pub probe: String,
    pub probe_error: Option<String>,
    pub handshake: Option<String>,
    pub handshake_age_s: Option<i64>,
    pub handshake_detail: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ChannelOut {
    pub channel: String,
    pub requests: i64,
    pub hits: i64,
    pub misses: i64,
    pub hit_rate: Option<f64>,
    pub backend_errors: i64,
    pub hit_samples: i64,
    pub hit_p50_ns: Option<i64>,
    pub hit_p99_ns: Option<i64>,
    pub miss_samples: i64,
    pub miss_p50_ns: Option<i64>,
    pub miss_p99_ns: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct FindingOut {
    #[serde(serialize_with = "ser_level")]
    pub level: i64,
    pub node: String,
    pub text: String,
}

/// A row of a paged feed: its `(at_unix_ms, id)` is where a page starting at it begins.
pub trait Keyed: Serialize {
    fn key(&self) -> (i64, i64);
}

impl Keyed for PageRow {
    fn key(&self) -> (i64, i64) {
        (self.at_unix_ms, self.id)
    }
}

impl Keyed for CrawlOut {
    fn key(&self) -> (i64, i64) {
        (self.at_unix_ms, self.id)
    }
}

/// [`health_page`] or [`crawl_page`]: `(conn, from_ms, to_ms, node, after, rows)`.
pub type PageFn<T> =
    fn(&Connection, i64, i64, Option<i64>, Option<(i64, i64)>, usize) -> rusqlite::Result<Vec<T>>;

/// One `/history` line, copied out of the database so the connection can close
/// before it is serialised.
#[derive(Debug, Serialize)]
pub struct PageRow {
    pub id: i64,
    #[serde(serialize_with = "ser_at")]
    pub at: i64,
    pub at_unix_ms: i64,
    pub node: String,
    #[serde(serialize_with = "ser_level")]
    pub level: i64,
    #[serde(serialize_with = "ser_level")]
    pub poll_level: i64,
    pub reachable: bool,
    pub status: String,
    pub detail: Option<String>,
    pub traffic_error: Option<String>,
    pub rtt_ms: Option<f64>,
    pub binary: Option<String>,
    pub version: Option<String>,
    pub hash: Option<String>,
    pub uptime_s: Option<i64>,
    pub host_uptime_s: Option<i64>,
    pub requests_total: Option<i64>,
    pub monitor_requests: Option<i64>,
    pub cache_hits: Option<i64>,
    pub cache_misses: Option<i64>,
    pub hit_rate: Option<f64>,
    pub backend_errors: Option<i64>,
    pub hit_samples: Option<i64>,
    pub hit_p50_ns: Option<i64>,
    pub hit_p99_ns: Option<i64>,
    pub miss_samples: Option<i64>,
    pub miss_p50_ns: Option<i64>,
    pub miss_p99_ns: Option<i64>,
    pub load_one: Option<f64>,
    pub cpus: Option<i64>,
    pub memory_used: Option<f64>,
    pub memory_total_bytes: Option<i64>,
    pub disk_used: Option<f64>,
    pub disk_total_bytes: Option<i64>,
    pub thermal_max_c: Option<f64>,
    pub cert_not_after_unix: Option<i64>,
    pub cert_expires_in_s: Option<i64>,
    pub tunnel: Option<TunnelOut>,
    pub log_reopen: Option<ReopenRow>,
    pub net: Vec<NetRow>,
    pub channels: Vec<ChannelOut>,
    pub findings: Vec<FindingOut>,
    pub other_findings: Vec<FindingOut>,
    #[serde(skip)]
    poll_id: i64,
}

const HEALTH_SELECT: &str =
    "SELECT h.id, h.at_ms, n.name, h.level, p.level, h.reachable, h.status, \
h.detail, h.traffic_error, h.rtt_ms, b.binary, b.version, b.hash, h.uptime_s, h.host_uptime_s, \
h.requests_total, h.monitor_requests, h.cache_hits, h.cache_misses, h.backend_errors, \
h.hit_samples, h.hit_p50_ns, h.hit_p99_ns, h.miss_samples, h.miss_p50_ns, h.miss_p99_ns, \
h.load_one, h.cpus, h.memory_used, h.memory_total_bytes, h.disk_used, h.disk_total_bytes, \
h.thermal_max_c, h.cert_not_after_unix, h.tunnel_rtt_ms, h.tunnel_rtt_n, h.tunnel_probe, \
h.tunnel_probe_error, h.tunnel_handshake, h.tunnel_handshake_age_s, h.tunnel_handshake_detail, \
h.reopen_state, h.reopen_age_s, h.reopen_last_unix, h.reopen_reopens, h.reopen_failures, \
h.reopen_last_failure_age_s, h.reopen_detail, h.poll_id \
FROM health h JOIN node n ON n.id = h.node_id JOIN poll p ON p.id = h.poll_id \
LEFT JOIN build b ON b.id = h.build_id \
WHERE h.at_ms >= ?1 AND h.at_ms < ?2 AND (?3 IS NULL OR h.node_id = ?3) \
AND (?4 IS NULL OR h.at_ms > ?4 OR (h.at_ms = ?4 AND h.id >= ?5)) \
ORDER BY h.at_ms, h.id LIMIT ?6";

#[cfg(test)]
thread_local! {
    /// Runs after a page's main query, so a test can change the file between
    /// the statements of one page.
    static AFTER_ROWS: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
}

fn after_rows() {
    #[cfg(test)]
    AFTER_ROWS.with(|h| {
        if let Some(f) = h.borrow_mut().as_mut() {
            f();
        }
    });
}

/// Begin a deferred read transaction unless the caller already holds one, so
/// every statement of a page sees the same file even if a prune commits.
fn snapshot(conn: &Connection) -> rusqlite::Result<Option<Transaction<'_>>> {
    if conn.is_autocommit() {
        conn.unchecked_transaction().map(Some)
    } else {
        Ok(None)
    }
}

fn none_if_empty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.is_empty())
}

/// One page of `/history` rows in `[from_ms, to_ms)`, at most `rows` of them,
/// starting at `after` when given.
pub fn health_page(
    conn: &Connection,
    from_ms: i64,
    to_ms: i64,
    node: Option<i64>,
    after: Option<(i64, i64)>,
    rows: usize,
) -> rusqlite::Result<Vec<PageRow>> {
    let _snapshot = snapshot(conn)?;
    let mut out: Vec<PageRow> = {
        let mut stmt = conn.prepare(HEALTH_SELECT)?;
        let (a_at, a_id) = after.unzip();
        let limit = iu(rows);
        let mapped = stmt.query_map(params![from_ms, to_ms, node, a_at, a_id, limit], |r| {
            let at: i64 = r.get(1)?;
            let hits: Option<i64> = r.get(17)?;
            let misses: Option<i64> = r.get(18)?;
            let cert: Option<i64> = r.get(33)?;
            let tunnel_probe: Option<String> = r.get(36)?;
            let tunnel = match tunnel_probe {
                None => None,
                Some(probe) => Some(TunnelOut {
                    rtt_ms: r.get(34)?,
                    rtt_n: r.get(35)?,
                    probe,
                    probe_error: r.get(37)?,
                    handshake: r.get(38)?,
                    handshake_age_s: r.get(39)?,
                    handshake_detail: r.get(40)?,
                }),
            };
            let reopen_state: Option<String> = r.get(41)?;
            let log_reopen = match reopen_state {
                None => None,
                Some(state) => Some(ReopenRow {
                    state,
                    age_s: r.get(42)?,
                    last_unix: r.get(43)?,
                    reopens: r.get(44)?,
                    failures: r.get(45)?,
                    last_failure_age_s: r.get(46)?,
                    detail: r.get(47)?,
                }),
            };
            Ok(PageRow {
                id: r.get(0)?,
                at,
                at_unix_ms: at,
                node: r.get(2)?,
                level: r.get(3)?,
                poll_level: r.get(4)?,
                reachable: r.get(5)?,
                status: r.get(6)?,
                detail: r.get(7)?,
                traffic_error: r.get(8)?,
                rtt_ms: r.get(9)?,
                binary: none_if_empty(r.get(10)?),
                version: none_if_empty(r.get(11)?),
                hash: none_if_empty(r.get(12)?),
                uptime_s: r.get(13)?,
                host_uptime_s: r.get(14)?,
                requests_total: r.get(15)?,
                monitor_requests: r.get(16)?,
                cache_hits: hits,
                cache_misses: misses,
                hit_rate: rate(hits, misses),
                backend_errors: r.get(19)?,
                hit_samples: r.get(20)?,
                hit_p50_ns: r.get(21)?,
                hit_p99_ns: r.get(22)?,
                miss_samples: r.get(23)?,
                miss_p50_ns: r.get(24)?,
                miss_p99_ns: r.get(25)?,
                load_one: r.get(26)?,
                cpus: r.get(27)?,
                memory_used: r.get(28)?,
                memory_total_bytes: r.get(29)?,
                disk_used: r.get(30)?,
                disk_total_bytes: r.get(31)?,
                thermal_max_c: r.get(32)?,
                cert_not_after_unix: cert,
                cert_expires_in_s: cert.map(|c| c - at / 1000),
                tunnel,
                log_reopen,
                net: Vec::new(),
                channels: Vec::new(),
                findings: Vec::new(),
                other_findings: Vec::new(),
                poll_id: r.get(48)?,
            })
        })?;
        mapped.collect::<rusqlite::Result<_>>()?
    };
    after_rows();
    let (Some(first), Some(last)) = (
        out.iter().map(|r| r.id).min(),
        out.iter().map(|r| r.id).max(),
    ) else {
        return Ok(out);
    };
    let index: HashMap<i64, usize> = out.iter().enumerate().map(|(k, r)| (r.id, k)).collect();
    {
        let mut stmt = conn.prepare(
            "SELECT hc.health_id, c.label, hc.requests, hc.hits, hc.misses, hc.backend_errors, \
             hc.hit_samples, hc.hit_p50_ns, hc.hit_p99_ns, hc.miss_samples, hc.miss_p50_ns, \
             hc.miss_p99_ns FROM health_channel hc JOIN channel c ON c.id = hc.channel_id \
             WHERE hc.health_id BETWEEN ?1 AND ?2 ORDER BY hc.health_id, c.label",
        )?;
        let mut rows = stmt.query(params![first, last])?;
        while let Some(r) = rows.next()? {
            let Some(&k) = index.get(&r.get::<_, i64>(0)?) else {
                continue;
            };
            let (hits, misses): (i64, i64) = (r.get(3)?, r.get(4)?);
            out[k].channels.push(ChannelOut {
                channel: r.get(1)?,
                requests: r.get(2)?,
                hits,
                misses,
                hit_rate: rate(Some(hits), Some(misses)),
                backend_errors: r.get(5)?,
                hit_samples: r.get(6)?,
                hit_p50_ns: r.get(7)?,
                hit_p99_ns: r.get(8)?,
                miss_samples: r.get(9)?,
                miss_p50_ns: r.get(10)?,
                miss_p99_ns: r.get(11)?,
            });
        }
    }
    {
        let mut stmt = conn.prepare(
            "SELECT hn.health_id, i.name, hn.rx_bytes, hn.rx_errs, hn.rx_drop, hn.tx_bytes, \
             hn.tx_errs, hn.tx_drop FROM health_net hn JOIN iface i ON i.id = hn.iface_id \
             WHERE hn.health_id BETWEEN ?1 AND ?2 ORDER BY hn.health_id, i.name",
        )?;
        let mut rows = stmt.query(params![first, last])?;
        while let Some(r) = rows.next()? {
            let Some(&k) = index.get(&r.get::<_, i64>(0)?) else {
                continue;
            };
            out[k].net.push(NetRow {
                iface: r.get(1)?,
                rx_bytes: r.get(2)?,
                rx_errs: r.get(3)?,
                rx_drop: r.get(4)?,
                tx_bytes: r.get(5)?,
                tx_errs: r.get(6)?,
                tx_drop: r.get(7)?,
            });
        }
    }
    let (p_first, p_last) = (
        out.iter().map(|r| r.poll_id).min().unwrap_or(0),
        out.iter().map(|r| r.poll_id).max().unwrap_or(0),
    );
    let mut by_poll: HashMap<i64, Vec<(i64, bool, String, String)>> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT poll_id, level, polled, node, text FROM poll_finding \
             WHERE poll_id BETWEEN ?1 AND ?2 ORDER BY poll_id, level DESC, id",
        )?;
        let mut rows = stmt.query(params![p_first, p_last])?;
        while let Some(r) = rows.next()? {
            by_poll.entry(r.get(0)?).or_default().push((
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
            ));
        }
    }
    for row in &mut out {
        for (level, polled, node, text) in by_poll.get(&row.poll_id).into_iter().flatten().cloned()
        {
            let f = FindingOut {
                level,
                node: node.clone(),
                text,
            };
            if polled && node == row.node {
                row.findings.push(f);
            } else if !polled {
                row.other_findings.push(f);
            }
        }
    }
    Ok(out)
}

/// One `/crawlers` line.
#[derive(Debug, Serialize)]
pub struct CrawlOut {
    pub id: i64,
    #[serde(serialize_with = "ser_at")]
    pub at: i64,
    pub at_unix_ms: i64,
    pub node: String,
    pub window_minutes: i64,
    pub total_requests: i64,
    pub status: BTreeMap<u16, i64>,
    pub forged_bot_requests: i64,
    pub forgers: Vec<String>,
    pub crawlers: Vec<SightingOut>,
    pub sightings_dropped: i64,
}

#[derive(Debug, Serialize)]
pub struct SightingOut {
    pub user_agent: String,
    pub requests: i64,
    pub client_ips: Vec<String>,
    pub paths: Vec<String>,
}

fn json_list(s: &str) -> Vec<String> {
    serde_json::from_str(s).unwrap_or_default()
}

/// One page of `/crawlers` rows, as [`health_page`] does for `/history`.
pub fn crawl_page(
    conn: &Connection,
    from_ms: i64,
    to_ms: i64,
    node: Option<i64>,
    after: Option<(i64, i64)>,
    rows: usize,
) -> rusqlite::Result<Vec<CrawlOut>> {
    let _snapshot = snapshot(conn)?;
    let mut out: Vec<CrawlOut> = {
        let mut stmt = conn.prepare(
            "SELECT w.id, w.at_ms, n.name, w.window_minutes, w.total_requests, \
             w.forged_bot_requests, w.forgers, w.sightings_dropped FROM crawl_window w JOIN node n ON n.id = w.node_id \
             WHERE w.at_ms >= ?1 AND w.at_ms < ?2 AND (?3 IS NULL OR w.node_id = ?3) \
             AND (?4 IS NULL OR w.at_ms > ?4 OR (w.at_ms = ?4 AND w.id >= ?5)) \
             ORDER BY w.at_ms, w.id LIMIT ?6",
        )?;
        let (a_at, a_id) = after.unzip();
        let mapped = stmt.query_map(params![from_ms, to_ms, node, a_at, a_id, iu(rows)], |r| {
            let at: i64 = r.get(1)?;
            Ok(CrawlOut {
                id: r.get(0)?,
                at,
                at_unix_ms: at,
                node: r.get(2)?,
                window_minutes: r.get(3)?,
                total_requests: r.get(4)?,
                status: BTreeMap::new(),
                forged_bot_requests: r.get(5)?,
                forgers: json_list(&r.get::<_, String>(6)?),
                crawlers: Vec::new(),
                sightings_dropped: r.get(7)?,
            })
        })?;
        mapped.collect::<rusqlite::Result<_>>()?
    };
    after_rows();
    let (Some(first), Some(last)) = (
        out.iter().map(|r| r.id).min(),
        out.iter().map(|r| r.id).max(),
    ) else {
        return Ok(out);
    };
    let index: HashMap<i64, usize> = out.iter().enumerate().map(|(k, r)| (r.id, k)).collect();
    {
        let mut stmt = conn.prepare(
            "SELECT window_id, code, requests FROM crawl_status WHERE window_id BETWEEN ?1 AND ?2",
        )?;
        let mut rows = stmt.query(params![first, last])?;
        while let Some(r) = rows.next()? {
            if let Some(&k) = index.get(&r.get::<_, i64>(0)?) {
                out[k].status.insert(r.get(1)?, r.get(2)?);
            }
        }
    }
    {
        let mut stmt = conn.prepare(
            "SELECT s.window_id, a.user_agent, s.requests, s.client_ips, s.paths \
             FROM crawl_sighting s JOIN agent a ON a.id = s.agent_id \
             WHERE s.window_id BETWEEN ?1 AND ?2 ORDER BY s.window_id, s.requests DESC, a.user_agent",
        )?;
        let mut rows = stmt.query(params![first, last])?;
        while let Some(r) = rows.next()? {
            if let Some(&k) = index.get(&r.get::<_, i64>(0)?) {
                out[k].crawlers.push(SightingOut {
                    user_agent: r.get(1)?,
                    requests: r.get(2)?,
                    client_ips: json_list(&r.get::<_, String>(3)?),
                    paths: json_list(&r.get::<_, String>(4)?),
                });
            }
        }
    }
    Ok(out)
}

/// Why [`fill_page`] failed.
#[derive(Debug)]
pub enum PageError {
    /// The database could not be read. The service answers 503.
    Db(rusqlite::Error),
    /// A row could not be written into the page.
    Build(anyhow::Error),
}

impl From<rusqlite::Error> for PageError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(e)
    }
}

/// Rows read from the database at a time while a page fills.
pub const CHUNK_ROWS: usize = 256;

/// Fill one page from `read`, and return it with the cursor of the next page.
///
/// All reads run in one read transaction. The rows come [`CHUNK_ROWS`] at a
/// time, each chunk with its children, and go into the page until it says it
/// is full or the range ends. A page of 4 MiB usually fills after a few
/// thousand rows, so the rest of the range is never read.
///
/// Each read asks for one row more than a chunk. The extra row starts the next
/// chunk, and when the page fills on it, it is where the next page starts.
pub fn fill_page<T: Keyed>(
    conn: &Connection,
    range: (i64, i64),
    node: Option<i64>,
    mut after: Option<(i64, i64)>,
    limits: m6_core::ndjson::PageLimits,
    mut read: impl FnMut(
        &Connection,
        i64,
        i64,
        Option<i64>,
        Option<(i64, i64)>,
        usize,
    ) -> rusqlite::Result<Vec<T>>,
) -> Result<(m6_core::ndjson::Page, Option<m6_core::ndjson::Cursor>), PageError> {
    use m6_core::ndjson::{Cursor, Page, Pushed};
    let build = |e: std::io::Error| PageError::Build(anyhow::Error::from(e));
    let _snapshot = snapshot(conn)?;
    let mut page = Page::new(limits);
    loop {
        let rows = read(conn, range.0, range.1, node, after, CHUNK_ROWS + 1)?;
        for r in rows.iter().take(CHUNK_ROWS) {
            if page.push(r).map_err(build)? == Pushed::Full {
                let (at_ms, id) = r.key();
                let next = Cursor::new(cursor_text(at_ms, id))
                    .map_err(|e| PageError::Build(anyhow::Error::from(e)))?;
                return Ok((page, Some(next)));
            }
        }
        match rows.get(CHUNK_ROWS) {
            Some(start) => after = Some(start.key()),
            None => return Ok((page, None)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::{self, Thresholds};
    use m6_core::host::{Disk, HostSnapshot, LoadAverage, Memory, MemorySource, NetDevice};
    use m6_core::monitoring::{LoggingHealth, TrafficReport};
    use m6_core::telemetry::ChannelSnapshot;

    const T0: i64 = 1_800_000_000_000;

    fn cfg(dir: &Path, mb: u64) -> Config {
        Config {
            file: dir.join("sub").join("history.sqlite3"),
            max_bytes: mb << 20,
        }
    }

    fn traffic(window: u64, crawlers: Vec<CrawlerReport>) -> TrafficReport {
        TrafficReport {
            node: "n".into(),
            window_minutes: window,
            total_requests: 900,
            status: [(200u16, 800u64), (404, 100)].into_iter().collect(),
            notable: vec![],
            heavy_hitters: vec![],
            crawlers,
            forged_bot_requests: 3,
            forgers: vec!["192.0.2.9".into()],
            probe_noise: vec![],
            logging: LoggingHealth {
                events_total: 5,
                seconds_since_last: Some(1),
                reopen: None,
            },
            firewall: None,
        }
    }

    fn crawler(ua: &str) -> CrawlerReport {
        CrawlerReport {
            user_agent: ua.into(),
            requests: 12,
            client_ips: vec!["192.0.2.1".into(), "192.0.2.2".into()],
            paths: vec!["/".into(), "/a".into()],
        }
    }

    fn channel(name: &str, samples: usize) -> ChannelSnapshot {
        ChannelSnapshot {
            channel: name.into(),
            requests: 100,
            hits: 80,
            misses: 20,
            backend_errors: 1,
            hit_samples: samples,
            hit_p50_ns: 900,
            hit_p99_ns: 4000,
            miss_samples: samples,
            miss_p50_ns: 70_000,
            miss_p99_ns: 90_000,
            ..Default::default()
        }
    }

    /// A node that answered everything, with two channels and the wg0 counters.
    fn reading(name: &str) -> NodeReading {
        let host = HostSnapshot {
            cpus: 4,
            uptime_s: Some(5000),
            load: Some(LoadAverage {
                one: 0.5,
                five: 0.4,
                fifteen: 0.3,
                running: 1,
                total: 100,
            }),
            memory: Some(Memory {
                total_bytes: 1000,
                available_bytes: 400,
                source: MemorySource::Host,
            }),
            disk: Some(Disk {
                total_bytes: 2000,
                available_bytes: 500,
            }),
            thermal: vec![m6_core::host::ThermalZone {
                name: "cpu".into(),
                celsius: 45.5,
            }],
            net: vec![NetDevice {
                name: "wg0".into(),
                rx_bytes: 10,
                rx_packets: 1,
                rx_errs: 2,
                rx_drop: 3,
                tx_bytes: 20,
                tx_packets: 1,
                tx_errs: 4,
                tx_drop: 5,
            }],
            ..Default::default()
        };
        let mut p = digest::tests::perf(host, vec![]);
        p.metrics.requests_total = 100;
        p.metrics.cache_hits_total = 80;
        p.metrics.cache_misses_total = 20;
        p.metrics.monitor_requests_total = 7;
        p.metrics.backend_errors_total = 1;
        p.metrics.hit_samples = 50;
        p.metrics.hit_p50_ns = 900;
        p.metrics.hit_p99_ns = 4000;
        p.metrics.miss_samples = 10;
        p.metrics.miss_p50_ns = 70_000;
        p.metrics.miss_p99_ns = 90_000;
        p.metrics.channels = vec![
            channel("http/2/external", 5),
            channel("http/1.1/internal", 0),
        ];
        NodeReading {
            name: name.into(),
            role: "origin".into(),
            url: "http://x".into(),
            health: None,
            health_status: None,
            perf: Some(p),
            perf_error: None,
            header_checks: vec![],
            rtt: Some(std::time::Duration::from_millis(12)),
            traffic: Some(traffic(60, vec![crawler("Bot/1.0")])),
            traffic_error: None,
            unreachable: None,
            tunnel: None,
        }
    }

    fn unreachable(name: &str) -> NodeReading {
        NodeReading {
            perf: None,
            traffic: None,
            rtt: None,
            unreachable: Some("connection refused".into()),
            ..reading(name)
        }
    }

    fn make(at_ms: i64, rs: &[NodeReading]) -> Batch {
        let d = digest::build(rs, &Thresholds::default(), "t".into());
        batch(at_ms, rs, &d)
    }

    fn open(dir: &Path) -> Store {
        Store::open(&cfg(dir, 64)).expect("open")
    }

    fn count(s: &Store, table: &str) -> i64 {
        s.conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn a_fresh_install_creates_the_directory_file_and_schema() {
        let dir = tempfile::tempdir().unwrap();
        let s = open(dir.path());
        assert!(cfg(dir.path(), 1).file.exists());
        assert_eq!(sqlite::schema_version(&s.conn).unwrap(), 1);
        let mut stmt = s
            .conn
            .prepare("SELECT name, strict FROM pragma_table_list WHERE schema = 'main' AND name NOT LIKE 'sqlite_%'")
            .unwrap();
        let tables: Vec<(String, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(tables.len(), 13, "{tables:?}");
        assert!(tables.iter().all(|(_, strict)| *strict == 1), "{tables:?}");
    }

    #[test]
    fn lookup_tables_are_append_only_and_history_tables_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let s = open(dir.path());
        for t in LOOKUP_TABLES {
            assert!(sqlite::is_append_only(&s.conn, t).unwrap(), "{t}");
        }
        for t in [
            "agent",
            "poll",
            "health",
            "health_channel",
            "crawl_window",
            "poll_finding",
        ] {
            assert!(!sqlite::is_append_only(&s.conn, t).unwrap(), "{t}");
        }
    }

    #[test]
    fn reopening_keeps_rows_and_applies_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(dir.path());
        s.write(&make(T0, &[reading("a")])).unwrap();
        drop(s);
        let s = open(dir.path());
        assert_eq!(count(&s, "poll"), 1);
        assert_eq!(count(&s, "health"), 1);
        assert_eq!(sqlite::schema_version(&s.conn).unwrap(), 1);
    }

    #[test]
    fn a_file_newer_than_the_code_is_fatal() {
        let dir = tempfile::tempdir().unwrap();
        drop(open(dir.path()));
        let c = Connection::open(cfg(dir.path(), 1).file).unwrap();
        c.pragma_update(None, "user_version", 2).unwrap();
        drop(c);
        let err = Store::open(&cfg(dir.path(), 1)).err().expect("refused");
        assert!(is_fatal(&err), "{err:#}");
        let err = start(Some(cfg(dir.path(), 1))).err().expect("refused");
        assert!(format!("{err:#}").contains("history.sqlite3"), "{err:#}");
    }

    #[test]
    fn a_row_carries_every_figure() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(dir.path());
        let mut r = reading("a");
        r.traffic = None;
        r.traffic_error = Some("404: not available".into());
        let b = make(T0, &[r]);
        assert_eq!(b.rows[0].channels.len(), 2);
        s.write(&b).unwrap();
        let (backend, monitor, hits, misses, mp50, mp99): (i64, i64, i64, i64, i64, i64) = s
            .conn
            .query_row(
                "SELECT backend_errors, monitor_requests, cache_hits, cache_misses, miss_p50_ns, \
                 miss_p99_ns FROM health",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            (backend, monitor, hits, misses, mp50, mp99),
            (1, 7, 80, 20, 70_000, 90_000)
        );
        let (load, cpus, mem, disk, thermal): (f64, i64, f64, f64, f64) = s
            .conn
            .query_row(
                "SELECT load_one, cpus, memory_used, disk_used, thermal_max_c FROM health",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!((load, cpus, mem, disk, thermal), (0.5, 4, 0.6, 0.75, 45.5));
        let wg_drop: i64 = s
            .conn
            .query_row("SELECT tx_drop FROM health_net", [], |r| r.get(0))
            .unwrap();
        assert_eq!(wg_drop, 5);
        // The node has a backend error, which is a Warn finding on that node.
        let poll_level: i64 = s
            .conn
            .query_row("SELECT level FROM poll", [], |r| r.get(0))
            .unwrap();
        let node_level: i64 = s
            .conn
            .query_row("SELECT level FROM health", [], |r| r.get(0))
            .unwrap();
        assert_eq!((poll_level, node_level), (1, 1));
        let te: Option<String> = s
            .conn
            .query_row("SELECT traffic_error FROM health", [], |r| r.get(0))
            .unwrap();
        assert_eq!(te.as_deref(), Some("404: not available"));
        assert_eq!(count(&s, "poll_finding"), 1);
    }

    #[test]
    fn tunnel_and_log_reopen_figures_are_stored() {
        use crate::tunnel::{Handshake, Probe, ProbeResult, TunnelReading};
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(dir.path());
        let mut r = reading("a");
        r.tunnel = Some(TunnelReading {
            address: "192.0.2.2:443".into(),
            key_prefix: None,
            probe: ProbeResult {
                samples: vec![std::time::Duration::from_micros(400); 3],
                outcome: Probe::Connected,
            },
            handshake: Handshake::At { age_s: 42 },
            file_age_s: Some(1),
        });
        s.write(&make(T0, &[r])).unwrap();
        let (rtt, n, probe, hs, age, state): (f64, i64, String, String, i64, String) = s
            .conn
            .query_row(
                "SELECT tunnel_rtt_ms, tunnel_rtt_n, tunnel_probe, tunnel_handshake, \
                 tunnel_handshake_age_s, reopen_state FROM health",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            (n, probe.as_str(), hs.as_str(), age),
            (3, "connected", "ok", 42)
        );
        assert!((rtt - 0.4).abs() < 1e-9);
        assert_eq!(state, "unknown");
    }

    #[test]
    fn an_unreachable_node_still_gets_a_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(dir.path());
        s.write(&make(T0, &[unreachable("down")])).unwrap();
        let (reach, status, detail, lvl, req, build): (
            i64,
            String,
            String,
            i64,
            Option<i64>,
            Option<i64>,
        ) = s
            .conn
            .query_row(
                "SELECT reachable, status, detail, level, requests_total, build_id FROM health",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            (reach, status.as_str(), detail.as_str(), lvl),
            (0, "unreachable", "connection refused", 2)
        );
        assert_eq!((req, build), (None, None));
    }

    #[test]
    fn percentiles_with_no_samples_are_null() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(dir.path());
        let mut r = reading("a");
        if let Some(p) = r.perf.as_mut() {
            p.metrics.hit_samples = 0;
            p.metrics.miss_samples = 0;
        }
        s.write(&make(T0, &[r])).unwrap();
        let n: i64 = s
            .conn
            .query_row(
                "SELECT count(*) FROM health WHERE hit_p50_ns IS NULL AND hit_p99_ns IS NULL \
                 AND miss_p50_ns IS NULL AND miss_p99_ns IS NULL AND hit_samples = 0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
        // The second channel has no samples, the first has.
        let nulls: i64 = s
            .conn
            .query_row(
                "SELECT count(*) FROM health_channel WHERE hit_p50_ns IS NULL AND miss_p99_ns IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(nulls, 1);
    }

    #[test]
    fn one_poll_is_one_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(dir.path());
        let mut b = make(T0, &[reading("a")]);
        let dup = b.rows[0].channels[0].clone();
        b.rows[0].channels.push(dup);
        assert!(s.write(&b).is_err());
        assert_eq!(count(&s, "poll"), 0);
        assert_eq!(count(&s, "health"), 0);
        assert_eq!(count(&s, "poll_finding"), 0);
        assert_eq!(count(&s, "crawl_window"), 0);
        s.write(&make(T0 + 1, &[reading("a")])).unwrap();
        assert_eq!(count(&s, "poll"), 1);
    }

    #[test]
    fn names_channels_ifaces_agents_and_builds_are_stored_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(dir.path());
        for k in 0..3 {
            s.write(&make(T0 + k * 30_000, &[reading("a"), reading("b")]))
                .unwrap();
        }
        assert_eq!(count(&s, "node"), 2);
        assert_eq!(count(&s, "channel"), 2);
        assert_eq!(count(&s, "iface"), 1);
        assert_eq!(count(&s, "agent"), 1);
        assert_eq!(count(&s, "build"), 1);
        assert_eq!(count(&s, "health"), 6);
    }

    #[test]
    fn a_new_build_gets_its_own_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(dir.path());
        s.write(&make(T0, &[reading("a")])).unwrap();
        let mut r = reading("a");
        if let Some(p) = r.perf.as_mut() {
            p.build.hash = "f".repeat(32);
        }
        s.write(&make(T0 + 30_000, &[r])).unwrap();
        assert_eq!(count(&s, "build"), 2);
        let distinct: i64 = s
            .conn
            .query_row("SELECT count(DISTINCT build_id) FROM health", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(distinct, 2);
    }

    #[test]
    fn crawlers_are_stored_once_per_window_per_node() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(dir.path());
        for k in 0..121 {
            s.write(&make(T0 + k * 30_000, &[reading("a")])).unwrap();
        }
        // Polls at 0 and 60 minutes. The poll at 3600 s is exactly one window on.
        assert_eq!(count(&s, "crawl_window"), 2);
        assert_eq!(count(&s, "crawl_sighting"), 2);
        assert_eq!(count(&s, "crawl_status"), 4);
        let (req, forged, forgers): (i64, i64, String) = s
            .conn
            .query_row(
                "SELECT total_requests, forged_bot_requests, forgers FROM crawl_window LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((req, forged, forgers.as_str()), (900, 3, "[\"192.0.2.9\"]"));
        let mut none = reading("a");
        none.traffic = None;
        s.write(&make(T0 + 10_000_000, &[none])).unwrap();
        assert_eq!(count(&s, "crawl_window"), 2);
    }

    #[test]
    fn the_crawler_tiling_resumes_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(dir.path());
        s.write(&make(T0, &[reading("a")])).unwrap();
        drop(s);
        let mut s = open(dir.path());
        s.write(&make(T0 + 30_000, &[reading("a")])).unwrap();
        assert_eq!(
            count(&s, "crawl_window"),
            1,
            "30 s later is the same window"
        );
        s.write(&make(T0 + 3_600_000, &[reading("a")])).unwrap();
        assert_eq!(count(&s, "crawl_window"), 2);
    }

    fn file_len(dir: &Path) -> u64 {
        std::fs::metadata(cfg(dir, 1).file).unwrap().len()
    }

    #[test]
    fn the_file_stays_under_its_cap_and_prunes_whole_polls() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(&cfg(dir.path(), 1)).unwrap();
        let mut pruned = 0;
        for k in 0..1500 {
            let mut rs = vec![reading("a"), reading("b"), reading("c")];
            for r in &mut rs {
                if let Some(p) = r.perf.as_mut() {
                    p.metrics.channels = (0..6)
                        .map(|n| channel(&format!("http/{n}/external"), 3))
                        .collect();
                }
            }
            pruned += s.write(&make(T0 + k * 30_000, &rs)).unwrap().pruned;
        }
        assert!(pruned > 0, "the cap was never reached");
        assert!(
            file_len(dir.path()) <= (1 << 20) + 65_536,
            "{}",
            file_len(dir.path())
        );
        let oldest: i64 = s
            .conn
            .query_row("SELECT min(at_ms) FROM poll", [], |r| r.get(0))
            .unwrap();
        assert!(oldest > T0, "the oldest poll should be gone");
        let orphans = s
            .conn
            .prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query_map([], |_| Ok(()))
            .unwrap()
            .count();
        assert_eq!(orphans, 0);
        // Whole polls: every remaining poll still has its three node rows.
        let partial: i64 = s
            .conn
            .query_row(
                "SELECT count(*) FROM poll p WHERE (SELECT count(*) FROM health h WHERE h.poll_id = p.id) != 3",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(partial, 0);
    }

    #[test]
    fn bytes_per_node_poll_is_measured() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(dir.path());
        let polls = 600;
        for k in 0..polls {
            let mut rs = vec![reading("a"), reading("b"), reading("c")];
            for r in &mut rs {
                r.traffic = None;
                if let Some(p) = r.perf.as_mut() {
                    p.metrics.requests_total += k as u64;
                    p.metrics.channels = (0..6)
                        .map(|n| channel(&format!("http/{n}/external"), 3))
                        .collect();
                }
            }
            s.write(&make(T0 + k * 30_000, &rs)).unwrap();
        }
        let used = sqlite::used_bytes(&s.conn).unwrap();
        let per = used as f64 / (polls * 3) as f64;
        eprintln!("bytes per node-poll (6 channels, 1 interface): {per:.0}");
        assert!(per < 1024.0, "{per}");
    }

    fn fill(s: &mut Store, n: i64) {
        for k in 0..n {
            s.write(&make(T0 + k * 1000, &[reading("a"), reading("b")]))
                .unwrap();
        }
    }

    #[test]
    fn the_page_is_half_open_ordered_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(dir.path());
        fill(&mut s, 10);
        let rows = health_page(&s.conn, T0 + 2000, T0 + 5000, None, None, 100).unwrap();
        let at: Vec<i64> = rows.iter().map(|r| r.at_unix_ms).collect();
        assert_eq!(at, [2000, 2000, 3000, 3000, 4000, 4000].map(|x| T0 + x));
        assert!(rows
            .windows(2)
            .all(|w| (w[0].at_unix_ms, w[0].id) < (w[1].at_unix_ms, w[1].id)));
        assert_eq!(
            health_page(&s.conn, T0, T0 + 10_000, None, None, 3)
                .unwrap()
                .len(),
            3
        );
        assert!(
            health_page(&s.conn, T0 + 20_000, T0 + 30_000, None, None, 3)
                .unwrap()
                .is_empty()
        );
        let r = &rows[0];
        assert_eq!(r.channels.len(), 2);
        assert_eq!(r.net[0].iface, "wg0");
        assert_eq!(r.hit_rate, Some(0.8));
    }

    #[test]
    fn the_cursor_returns_the_next_page_without_gaps_or_repeats() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(dir.path());
        for k in 0..617 {
            s.write(&make(T0 + k * 1000, &[reading("a"), reading("b")]))
                .unwrap();
        }
        let mut seen = Vec::new();
        let mut after = None;
        let mut pages = 0;
        loop {
            let rows = health_page(&s.conn, T0, T0 + 10_000_000, None, after, 501).unwrap();
            pages += 1;
            for r in rows.iter().take(500) {
                seen.push(r.id);
            }
            match rows.get(500) {
                Some(next) => {
                    after = parse_cursor(&cursor_text(next.at_unix_ms, next.id));
                    assert!(after.is_some());
                }
                None => break,
            }
        }
        assert_eq!(pages, 3);
        assert_eq!(seen.len(), 1234);
        assert!(seen.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn the_query_filters_by_node() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(dir.path());
        fill(&mut s, 4);
        let b = node_id(&s.conn, "b").unwrap().unwrap();
        let rows = health_page(&s.conn, T0, T0 + 10_000, Some(b), None, 100).unwrap();
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|r| r.node == "b"));
        assert_eq!(node_id(&s.conn, "zzz").unwrap(), None);
    }

    #[test]
    fn a_page_read_releases_the_lock_when_it_returns() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(dir.path());
        fill(&mut s, 3);
        let reader = sqlite::open_reader(&cfg(dir.path(), 1).file).unwrap();
        let rows = health_page(&reader, T0, T0 + 10_000, None, None, 100).unwrap();
        // The rows are in memory. A second connection can write now, with a
        // short busy timeout, while the reader connection is still open.
        let w = sqlite::OpenOptions::default()
            .busy_timeout(std::time::Duration::from_millis(200))
            .open(&cfg(dir.path(), 1).file)
            .unwrap();
        w.execute("INSERT INTO node (name) VALUES ('late')", [])
            .unwrap();
        assert!(!rows.is_empty());
    }

    #[test]
    fn a_line_has_exactly_the_documented_keys() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(dir.path());
        s.write(&make(T0, &[reading("a")])).unwrap();
        let row = &health_page(&s.conn, T0, T0 + 1000, None, None, 10).unwrap()[0];
        let v = serde_json::to_value(row).unwrap();
        let keys = |v: &Value| -> Vec<String> {
            let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        let words = |s: &str| -> Vec<String> {
            let mut w: Vec<String> = s.split_whitespace().map(String::from).collect();
            w.sort();
            w
        };
        assert_eq!(
            keys(&v),
            words(
                "id at at_unix_ms node level poll_level reachable status detail traffic_error \
                 rtt_ms binary version hash uptime_s host_uptime_s requests_total monitor_requests \
                 cache_hits cache_misses hit_rate backend_errors hit_samples hit_p50_ns hit_p99_ns \
                 miss_samples miss_p50_ns miss_p99_ns load_one cpus memory_used memory_total_bytes \
                 disk_used disk_total_bytes thermal_max_c cert_not_after_unix cert_expires_in_s \
                 tunnel log_reopen net channels findings other_findings"
            )
        );
        assert_eq!(v["at"], "2027-01-15T08:00:00.000Z");
        assert!(v["tunnel"].is_null());
        assert!(
            v["cert_expires_in_s"].is_null(),
            "an absent figure is null, never 0"
        );
        assert_eq!(
            keys(&v["channels"][0]),
            words(
                "channel requests hits misses hit_rate backend_errors hit_samples hit_p50_ns \
                 hit_p99_ns miss_samples miss_p50_ns miss_p99_ns"
            )
        );
        assert_eq!(
            keys(&v["net"][0]),
            words("iface rx_bytes rx_errs rx_drop tx_bytes tx_errs tx_drop")
        );

        let c = &crawl_page(&s.conn, T0, T0 + 1000, None, None, 10).unwrap()[0];
        let v = serde_json::to_value(c).unwrap();
        assert_eq!(
            keys(&v),
            words(
                "id at at_unix_ms node window_minutes total_requests status \
                 forged_bot_requests forgers crawlers sightings_dropped"
            )
        );
        assert_eq!(v["status"]["404"], 100);
        assert_eq!(
            keys(&v["crawlers"][0]),
            words("user_agent requests client_ips paths")
        );
        assert_eq!(v["crawlers"][0]["user_agent"], "Bot/1.0");
        assert_eq!(v["crawlers"][0]["paths"][1], "/a");
    }

    #[test]
    fn findings_are_stored_and_served_per_node_and_per_poll() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(dir.path());
        let rs = [reading("a"), unreachable("b")];
        let mut d = digest::build(&rs, &Thresholds::default(), "t".into());
        d.add_finding(Finding {
            level: Level::Warn,
            node: "monitor".into(),
            text: "history is slow".into(),
        });
        s.write(&batch(T0, &rs, &d)).unwrap();
        let rows = health_page(&s.conn, T0, T0 + 1000, None, None, 10).unwrap();
        let a = rows.iter().find(|r| r.node == "a").unwrap();
        let b = rows.iter().find(|r| r.node == "b").unwrap();
        assert_eq!(a.findings.len(), 1);
        assert!(a.findings[0].text.contains("backend errors"));
        assert_eq!(b.findings.len(), 1);
        assert!(b.findings[0].text.starts_with("unreachable"));
        for r in [a, b] {
            assert_eq!(r.other_findings.len(), 1);
            assert_eq!(r.other_findings[0].text, "history is slow");
            assert_eq!(r.poll_level, 2);
        }
        assert_eq!(a.level, 1);
        assert_eq!(b.level, 2);
    }

    #[test]
    fn record_never_blocks_when_the_writer_is_stuck() {
        let dir = tempfile::tempdir().unwrap();
        let shared = new_shared(Some(cfg(dir.path(), 1)));
        let (tx, _rx) = sync_channel(QUEUE_POLLS);
        let rec = Recorder {
            tx: Some(tx),
            shared: Arc::clone(&shared),
        };
        let h = HistoryHandle(shared);
        let b = make(T0, &[reading("a")]);
        let started = Instant::now();
        for _ in 0..QUEUE_POLLS + 3 {
            rec.record(b.clone());
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        let d = h.digest();
        assert_eq!(d.dropped, 3);
        assert_eq!(d.state, "behind");
    }

    fn writer(c: Config) -> (Writer, HistoryHandle) {
        let shared = new_shared(Some(c.clone()));
        (
            Writer {
                store: None,
                cfg: c,
                shared: Arc::clone(&shared),
            },
            HistoryHandle(shared),
        )
    }

    #[test]
    fn a_failed_open_is_reported_and_the_next_poll_retries() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("sub");
        std::fs::write(&blocker, b"a file where the directory should be").unwrap();
        let (mut w, h) = writer(cfg(dir.path(), 1));
        let b = make(T0, &[reading("a")]);
        w.handle(&b);
        let d = h.digest();
        assert_eq!(d.state, "not_open");
        assert_eq!(d.failures, 1);
        assert!(finding(&d).is_some());
        std::fs::remove_file(&blocker).unwrap();
        w.handle(&b);
        let d = h.digest();
        assert_eq!((d.state, d.writes), ("ok", 1));
        assert_eq!(d.last_error, None);
        assert!(finding(&d).is_none());
    }

    #[test]
    fn a_failed_write_is_failing_until_one_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let (mut w, h) = writer(cfg(dir.path(), 1));
        w.handle(&make(T0, &[reading("a")]));
        let mut bad = make(T0 + 1, &[reading("a")]);
        let dup = bad.rows[0].channels[0].clone();
        bad.rows[0].channels.push(dup);
        w.handle(&bad);
        assert_eq!(h.digest().state, "failing");
        w.handle(&make(T0 + 2, &[reading("a")]));
        let d = h.digest();
        assert_eq!((d.state, d.writes, d.failures), ("ok", 2, 1));
    }

    #[test]
    fn config_defaults_limits_and_relative_paths() {
        let site = Path::new("/srv/site");
        let toml = |s: &str| -> Value { serde_json::from_str(s).unwrap() };
        assert_eq!(config_from(None, site).unwrap(), None);
        assert_eq!(config_from(Some(&toml("{}")), site).unwrap(), None);
        let c = config_from(Some(&toml(r#"{"history_file":"h.db"}"#)), site)
            .unwrap()
            .unwrap();
        assert_eq!(c.file, Path::new("/srv/site/h.db"));
        assert_eq!(c.max_bytes, 256 << 20);
        let c = config_from(
            Some(&toml(r#"{"history_file":"/var/h.db","history_max_mb":1}"#)),
            site,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            (c.file.as_path(), c.max_bytes),
            (Path::new("/var/h.db"), 1 << 20)
        );
        for bad in [
            r#"{"history_file":""}"#,
            r#"{"history_file":3}"#,
            r#"{"history_file":"h","history_max_mb":0}"#,
            r#"{"history_file":"h","history_max_mb":1048577}"#,
            r#"{"history_file":"h","history_max_mb":"big"}"#,
        ] {
            let e = config_from(Some(&toml(bad)), site).unwrap_err().to_string();
            assert!(e.contains("history_"), "{bad}: {e}");
        }
        assert!(config_from(
            Some(&toml(r#"{"history_file":"h","history_max_mb":1048576}"#)),
            site
        )
        .is_ok());
    }

    #[test]
    fn node_param_takes_one_decoded_value() {
        assert_eq!(node_param("from=1&to=2"), Ok(None));
        assert_eq!(node_param("node=edge%2Da&x=1"), Ok(Some("edge-a".into())));
        assert!(node_param("node=a&node=b").is_err());
        assert!(node_param("node=").is_err());
        assert!(node_param("node").is_err());
    }

    #[test]
    fn history_digest_states_and_their_findings() {
        let off = HistoryHandle(new_shared(None));
        assert_eq!(off.digest().state, "off");
        assert!(finding(&off.digest()).is_none());

        let dir = tempfile::tempdir().unwrap();
        let shared = new_shared(Some(cfg(dir.path(), 1)));
        let h = HistoryHandle(Arc::clone(&shared));
        let f = |h: &HistoryHandle| finding(&h.digest()).map(|f| (f.level, f.node, f.text));
        assert_eq!(h.digest().state, "ok");
        assert_eq!(f(&h), None);

        shared.status().open_error = Some("denied".into());
        let path = cfg(dir.path(), 1).file.display().to_string();
        assert_eq!(
            f(&h).unwrap(),
            (
                Level::Warn,
                "monitor".into(),
                format!(
                    "history file {path} could not be opened, so no history is being kept: denied"
                )
            )
        );
        shared.status().open_error = None;
        shared.status().last_error = Some("disk full".into());
        assert_eq!(
            f(&h).unwrap().2,
            "the last history write failed, so that poll is missing from the history: disk full"
        );
        shared.status().last_error = None;
        shared.dropped.store(2, Ordering::Relaxed);
        assert_eq!(
            f(&h).unwrap().2,
            "the history writer is behind and has dropped 2 poll(s) since start: is the disk slow or the file locked?"
        );
        shared.stopped.store(true, Ordering::Relaxed);
        assert_eq!(
            f(&h).unwrap().2,
            "the history writer has stopped, so no history is being kept"
        );
        assert_eq!(h.digest().state, "stopped");
    }

    fn crawl_reading(name: &str, crawlers: Vec<CrawlerReport>) -> NodeReading {
        let mut r = reading(name);
        r.traffic = Some(traffic(60, crawlers));
        r
    }

    fn sighting(ua: String, requests: u64) -> CrawlerReport {
        CrawlerReport {
            user_agent: ua,
            requests,
            ..crawler("x")
        }
    }

    #[test]
    fn one_window_stores_at_most_100_sightings_of_at_most_256_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(dir.path());
        let crawlers: Vec<CrawlerReport> = (0..150u64)
            .map(|k| sighting(format!("{k:04}-{}", "\u{e9}".repeat(300)), k + 1))
            .collect();
        s.write(&make(T0, &[crawl_reading("a", crawlers)])).unwrap();
        assert_eq!(count(&s, "crawl_sighting"), 100);
        assert_eq!(count(&s, "agent"), 100);
        let longest: i64 = s
            .conn
            .query_row(
                "SELECT max(length(CAST(user_agent AS BLOB))) FROM agent",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(longest <= 256, "{longest}");
        let fewest: i64 = s
            .conn
            .query_row("SELECT min(requests) FROM crawl_sighting", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fewest, 51, "the busiest 100 are kept");
        let dropped: i64 = s
            .conn
            .query_row("SELECT sightings_dropped FROM crawl_window", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(dropped, 50);
        let rows = crawl_page(&s.conn, T0, T0 + 1000, None, None, 10).unwrap();
        assert_eq!(rows[0].sightings_dropped, 50);
        assert_eq!(rows[0].crawlers.len(), 100);
    }

    #[test]
    fn agents_that_match_after_truncation_are_one_sighting() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(dir.path());
        let prefix = "p".repeat(300);
        let crawlers = vec![
            sighting(format!("{prefix}one"), 5),
            sighting(format!("{prefix}two"), 7),
        ];
        s.write(&make(T0, &[crawl_reading("a", crawlers)])).unwrap();
        assert_eq!(count(&s, "crawl_sighting"), 1);
        let (n, dropped): (i64, i64) = s
            .conn
            .query_row(
                "SELECT s.requests, w.sightings_dropped FROM crawl_sighting s \
                 JOIN crawl_window w ON w.id = s.window_id",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((n, dropped), (12, 0));
    }

    #[test]
    fn unique_agents_across_many_windows_keep_the_file_under_its_cap() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(&cfg(dir.path(), 8)).unwrap();
        let mut pruned = 0;
        let windows = 1500i64;
        for k in 0..windows {
            let crawlers = (0..20u64)
                .map(|j| sighting(format!("bot-{k}-{j}-{}", "x".repeat(300)), 1 + j))
                .collect();
            let w = s
                .write(&make(T0 + k * 3_600_000, &[crawl_reading("a", crawlers)]))
                .unwrap();
            assert!(!w.cap_unmet, "window {k}: the cap could not be met");
            pruned += w.pruned;
        }
        assert!(pruned > 0, "the cap was never reached");
        assert!(
            file_len(dir.path()) <= (8 << 20) + 65_536,
            "{}",
            file_len(dir.path())
        );
        let (polls, newest): (i64, i64) = s
            .conn
            .query_row("SELECT count(*), max(at_ms) FROM poll", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert!(polls >= 100, "{polls}");
        assert_eq!(newest, T0 + (windows - 1) * 3_600_000);
        let unused: i64 = s
            .conn
            .query_row(
                "SELECT count(*) FROM agent WHERE id NOT IN (SELECT agent_id FROM crawl_sighting)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unused, 0, "an agent nothing references was kept");
    }

    /// Run `f` with a prune of every poll, by a second connection, tried
    /// after the main query of each page read. The file uses a rollback
    /// journal, so the prune fails at once while the page holds a read
    /// transaction and succeeds when the page reads in autocommit.
    fn with_prune_between_statements<R>(path: &Path, f: impl FnOnce() -> R) -> R {
        let path = path.to_path_buf();
        AFTER_ROWS.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || {
                let w = sqlite::OpenOptions::default()
                    .busy_timeout(std::time::Duration::from_millis(1))
                    .open(&path)
                    .unwrap();
                let _ = w.execute("DELETE FROM poll", []);
            }));
        });
        let r = f();
        AFTER_ROWS.with(|h| *h.borrow_mut() = None);
        r
    }

    #[test]
    fn a_history_page_reads_one_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(dir.path());
        let rs = [reading("a"), unreachable("b")];
        let d = digest::build(&rs, &Thresholds::default(), "t".into());
        s.write(&batch(T0, &rs, &d)).unwrap();
        let file = cfg(dir.path(), 1).file;
        let reader = sqlite::open_reader(&file).unwrap();
        let rows = with_prune_between_statements(&file, || {
            health_page(&reader, T0, T0 + 1000, None, None, 10).unwrap()
        });
        let a = rows.iter().find(|r| r.node == "a").unwrap();
        assert_eq!(a.channels.len(), 2, "channels of a pruned poll are kept");
        assert_eq!(a.net.len(), 1);
        assert_eq!(a.findings.len(), 1);
    }

    #[test]
    fn a_crawler_page_reads_one_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(dir.path());
        s.write(&make(T0, &[reading("a")])).unwrap();
        let file = cfg(dir.path(), 1).file;
        let reader = sqlite::open_reader(&file).unwrap();
        let rows = with_prune_between_statements(&file, || {
            crawl_page(&reader, T0, T0 + 1000, None, None, 10).unwrap()
        });
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status.len(), 2);
        assert_eq!(rows[0].crawlers.len(), 1);
    }

    /// Follow every cursor of a `/history` feed and return the rows of each
    /// page and the rows the database was asked for.
    fn walk(
        conn: &Connection,
        node: Option<i64>,
        limits: m6_core::ndjson::PageLimits,
    ) -> (Vec<usize>, usize) {
        let mut asked = 0;
        let mut after = None;
        let mut pages = Vec::new();
        loop {
            let (page, next) = fill_page(
                conn,
                (T0, T0 + 10_000_000),
                node,
                after,
                limits,
                |c, f, t, n, a, rows| {
                    let r = health_page(c, f, t, n, a, rows);
                    asked += r.as_ref().map_or(0, Vec::len);
                    r
                },
            )
            .ok()
            .unwrap();
            pages.push(page.rows());
            match next {
                Some(c) => after = Some(parse_cursor(c.as_str()).unwrap()),
                None => return (pages, asked),
            }
        }
    }

    fn fill_two_nodes(s: &mut Store, polls: i64) {
        for k in 0..polls {
            s.write(&make(T0 + k * 1000, &[reading("a"), reading("b")]))
                .unwrap();
        }
    }

    #[test]
    fn a_page_that_fills_mid_chunk_reads_one_chunk_and_loses_no_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(dir.path());
        fill_two_nodes(&mut s, 617);
        let reader = sqlite::open_reader(&cfg(dir.path(), 1).file).unwrap();
        // 1234 rows. Pages of 300 rows fill in the middle of the second chunk.
        let (pages, asked) = walk(
            &reader,
            None,
            m6_core::ndjson::PageLimits::new(1 << 20, 300),
        );
        assert_eq!(pages, [300, 300, 300, 300, 34]);
        assert_eq!(pages.iter().sum::<usize>(), 1234);
        // Each page reads two chunks of 257, never the 10,001 rows of a full page.
        assert!(asked <= 5 * 2 * (CHUNK_ROWS + 1), "{asked}");
    }

    #[test]
    fn a_page_that_fills_on_bytes_stops_reading() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(dir.path());
        fill_two_nodes(&mut s, 617);
        let reader = sqlite::open_reader(&cfg(dir.path(), 1).file).unwrap();
        let (pages, asked) = walk(
            &reader,
            None,
            m6_core::ndjson::PageLimits::new(40_000, 10_000),
        );
        assert_eq!(pages.iter().sum::<usize>(), 1234);
        assert!(pages.iter().all(|n| *n < CHUNK_ROWS), "{pages:?}");
        assert!(asked <= pages.len() * (CHUNK_ROWS + 1), "{asked}");
    }

    #[test]
    fn a_range_that_ends_exactly_on_a_chunk_has_no_next_page() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(dir.path());
        fill_two_nodes(&mut s, 128);
        let reader = sqlite::open_reader(&cfg(dir.path(), 1).file).unwrap();
        let (pages, _) = walk(&reader, None, m6_core::ndjson::PageLimits::default());
        assert_eq!(pages, [256]);
    }
}
