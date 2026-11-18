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
//! On Unix the monitor creates a missing directory with mode 0700 and the file
//! with mode 0600, and sets an existing file to 0600. A directory that already
//! exists keeps its mode. The file holds client addresses, so only the
//! monitor's user should read it.
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
//!   figures;
//! - `health_channel` rows (one per channel with traffic) and `health_net`
//!   rows (one per network interface);
//! - once per node per traffic window, one `crawl_window` row with the
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
//! The traffic document is a rolling summary over `window_minutes`, read from
//! the journal at every `poll_interval_s`. Storing every poll would repeat each
//! sighting about 120 times. A node's window is stored when it has none yet, or
//! when at least `window_minutes` have passed since its last stored one, so the
//! stored windows tile time without overlap. A restart resumes the tiling from
//! the file.
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
//! # Writing is the poll's own work
//!
//! The poll builds a batch once per interval and hands it to the loop's own
//! [`History`] value, which writes every held batch oldest first, each in one
//! short transaction. A transaction that meets a locked file returns
//! `SQLITE_BUSY` at once and leaves its batch, and the ones behind it, for the
//! next poll, so the loop never waits on the file. A hold of more than 8 polls
//! drops the oldest and counts it. A write that fails for any other reason
//! drops its batch, closes the connection and reports `failing`, and the next
//! poll opens it again.
//!
//! A prune that fails, or that deletes every earlier poll and is still over the
//! cap, does not lose the poll just written. The states are `prune_failed` and
//! `over_cap`, each with its own Warn, and the next write that prunes cleanly
//! clears them. The state `behind` means a batch is held for a locked file.
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
//!
//! [[route]]
//! path    = "/crawlers"
//! backend = "monitor"
//! require = "role:<role>"
//! ```
//!
//! The monitor sets `Cache-Control: no-store` on every page itself, so a route
//! needs no `cache` key.
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
//! traffic_error, rtt_ms, binary, version, uptime_s, host_uptime_s,
//! requests_total, monitor_requests, cache_hits, cache_misses, hit_rate,
//! backend_errors, hit_samples, hit_p50_ns, hit_p99_ns, miss_samples,
//! miss_p50_ns, miss_p99_ns, load_one, cpus, memory_used, memory_total_bytes,
//! disk_used, disk_total_bytes, thermal_max_c, cert_not_after_unix,
//! cert_expires_in_s, tunnel, net, channels, findings,
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
//! journal, so a write that meets a page read fails `SQLITE_BUSY` at once and
//! its poll is held for the next one rather than lost.
//!
//! # Caveats
//!
//! Channel counters separate the monitor's own polls and header checks (issue
//! #107) into `http/<version>/monitor`. `monitor_requests` on each node row
//! says how many.
//!
//! The node's own periodic stats log is not the history. It lives in the
//! node's journal, which is capped, it is a 10 second series with no channels,
//! no reachability, no host figures and no tunnel, and it disappears exactly
//! when a node is down, which is when a history is wanted.
//!
//! To start fresh, stop the service, delete the file and start it again.
//! Migrations 1, 2 and 3 are permanent, and a later change adds migration 4.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
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
  tunnel_handshake TEXT, tunnel_handshake_age_s INTEGER, tunnel_handshake_detail TEXT) STRICT;
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

/// The index the last-200 read answers from. Migration 2 adds it to a file
/// made by release 1, and it holds only the rows that count as a 200, so the
/// read touches one row per node however long the history is.
const SCHEMA_V2: &str = "
CREATE INDEX IF NOT EXISTS health_ok_node_at ON health (node_id, at_ms)
  WHERE reachable = 1 AND status = 'ok';
";

/// The `build` table without `hash`.
///
/// `hash` held the md5 of the node's executable, and then the commit from the
/// version's build metadata, which `version` already carries. A build is now
/// `(binary, version)`. `ALTER TABLE build DROP COLUMN hash` is refused because
/// the column is in the table's `UNIQUE` constraint, so the table is rebuilt the
/// way SQLite documents for a change `ALTER TABLE` cannot make: a new table, the
/// rows copied with their ids, the old one dropped and the new one renamed.
/// Two old rows that differ only in `hash` become one, the first by id, and a
/// `health` row that named the second names the first.
const SCHEMA_V3: &str = "
CREATE TABLE build_v3 (id INTEGER PRIMARY KEY, binary TEXT NOT NULL, version TEXT NOT NULL,
                       UNIQUE (binary, version)) STRICT;
INSERT OR IGNORE INTO build_v3 (id, binary, version) SELECT id, binary, version FROM build ORDER BY id;
UPDATE health SET build_id = (
    SELECT n.id FROM build o JOIN build_v3 n ON n.binary = o.binary AND n.version = o.version
    WHERE o.id = health.build_id)
  WHERE build_id IS NOT NULL AND build_id NOT IN (SELECT id FROM build_v3);
DROP TABLE build;
ALTER TABLE build_v3 RENAME TO build;
";

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: SCHEMA_V1,
    },
    Migration {
        version: 2,
        sql: SCHEMA_V2,
    },
    Migration {
        version: 3,
        sql: SCHEMA_V3,
    },
];

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
    /// The binary and its full version. The version carries the commit as
    /// its build metadata, so the pair names one build.
    pub build: Option<(String, String)>,
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
    let build = (n.binary.is_some() || n.version.is_some())
        .then(|| (present(&n.binary), present(&n.version)));
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
        thermal_max_c: n.thermal_max_c.map(|t| t as f64),
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
    /// The id of the poll just written. Pruning never deletes it.
    poll_id: i64,
    pruned: u64,
    cap_unmet: bool,
    /// Why pruning failed. The poll itself was written.
    prune_error: Option<String>,
    used_bytes: u64,
    file_bytes: u64,
    oldest_ms: Option<i64>,
    newest_ms: Option<i64>,
}

/// Create the directory (mode 0700 when new) and the empty file (mode 0600)
/// before SQLite does, and tighten a file left by an earlier start. SQLite
/// gives its journal the mode of the database file.
fn make_private(file: &Path) -> anyhow::Result<()> {
    m6_core::platform::create_private(file)
        .map_err(|e| anyhow::anyhow!("creating {}: {e}", file.display()))
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
        make_private(&cfg.file)?;
        let mut conn = sqlite::open(&cfg.file)?;
        sqlite::migrate(&mut conn, MIGRATIONS)?;
        for t in LOOKUP_TABLES {
            sqlite::append_only(&conn, t)?;
        }
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS agent_append_only_update; \
             DROP TRIGGER IF EXISTS agent_append_only_delete;",
        )?;
        // After the open a write never waits: a statement that meets another
        // connection returns SQLITE_BUSY at once and its poll stays held for
        // the next one. The open and its migrations keep the ordinary wait.
        conn.busy_timeout(std::time::Duration::ZERO)?;
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
                "INSERT INTO build (binary, version) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
                "SELECT id FROM build WHERE binary = ?1 AND version = ?2",
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

    /// When each node last answered `/health` with 200, by node name.
    ///
    /// One row per node, read from the partial index `health_ok_node_at`:
    /// each node's newest row is the first entry of its range, so the cost is
    /// the number of nodes and not the length of the history. It runs when the
    /// digest is built, so the age the check prints is the age in the file
    /// rather than a number kept in memory and able to drift from it.
    fn last_ok_at(&self) -> rusqlite::Result<HashMap<String, i64>> {
        let mut stmt = self.conn.prepare(LAST_OK_SQL)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect();
        rows
    }

    /// Write one poll in one transaction, then prune. On error the caller drops
    /// this store, so a rolled-back id is never reused from a cache.
    fn write(&mut self, b: &Batch) -> anyhow::Result<Written> {
        let r = self.insert(b);
        if r.is_err() {
            self.clear_caches();
        }
        let mut w = r?;
        // The poll is committed. A prune that fails is reported on its own.
        let keep_from = w.poll_id;
        let pruned = self.prune(&mut w, keep_from);
        let measured = self.measure(&mut w);
        if let Err(e) = pruned.and(measured) {
            w.prune_error = Some(format!("{e:#}"));
        }
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
        w.poll_id = poll_id;
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
                Some((bin, ver)) => Some(self.builds.id(&tx, &[bin, ver])?),
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
    /// with each batch the user agents no sighting names any more. A poll with
    /// an id of `keep_from` or more is never deleted.
    fn prune(&mut self, w: &mut Written, keep_from: i64) -> anyhow::Result<()> {
        let mut used = sqlite::used_bytes(&self.conn)?;
        if used > self.max_bytes {
            let target = self.max_bytes - self.max_bytes / 20;
            while used > target {
                let tx = self
                    .conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let n = tx.execute(
                    "DELETE FROM poll WHERE id IN \
                     (SELECT id FROM poll WHERE id < ?1 ORDER BY id LIMIT ?2)",
                    params![keep_from, PRUNE_BATCH],
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
        Ok(())
    }

    /// Fill in the figures the status shows.
    fn measure(&mut self, w: &mut Written) -> anyhow::Result<()> {
        w.used_bytes = sqlite::used_bytes(&self.conn)?;
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
tunnel_probe_error, tunnel_handshake, tunnel_handshake_age_s, tunnel_handshake_detail) \
VALUES (:poll_id, :at_ms, :node_id, :level, :reachable, \
:status, :detail, :traffic_error, :rtt_ms, :build_id, :uptime_s, :host_uptime_s, :requests_total, \
:monitor_requests, :cache_hits, :cache_misses, :backend_errors, :hit_samples, :hit_p50_ns, \
:hit_p99_ns, :miss_samples, :miss_p50_ns, :miss_p99_ns, :load_one, :cpus, :memory_used, \
:memory_total_bytes, :disk_used, :disk_total_bytes, :thermal_max_c, :cert_not_after_unix, \
:tunnel_rtt_ms, :tunnel_rtt_n, :tunnel_probe, :tunnel_probe_error, :tunnel_handshake, \
:tunnel_handshake_age_s, :tunnel_handshake_detail)";

// ── status ───────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Status {
    open_error: Option<String>,
    last_error: Option<String>,
    prune_error: Option<String>,
    over_cap: bool,
    writes: u64,
    crawl_windows: u64,
    failures: u64,
    pruned: u64,
    last_write: Option<Instant>,
    used_bytes: Option<u64>,
    file_bytes: Option<u64>,
    oldest_ms: Option<i64>,
    newest_ms: Option<i64>,
}

/// The age of one node's last `/health` answer of 200, from the history.
///
/// A node missing from the list has never answered 200 in the history that is
/// being kept, which the check reports as an absence rather than as an age.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LastOk {
    pub node: String,
    pub age_s: u64,
}

/// Each node's newest 200 row. The `WHERE` repeats the index's own, which is
/// what lets the planner use the partial index.
const LAST_OK_SQL: &str = "SELECT n.name, \
    (SELECT h.at_ms FROM health h WHERE h.node_id = n.id AND h.reachable = 1 \
       AND h.status = 'ok' ORDER BY h.at_ms DESC LIMIT 1) AS at_ms \
    FROM node n WHERE at_ms IS NOT NULL";

/// The wall clock in milliseconds. The one clock helper in the crate: the
/// digest's ages, the poll's stamp and the tests all read it.
pub(crate) fn now_ms() -> i64 {
    m6_core::platform::wall()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// The history as the digest reports it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct HistoryDigest {
    /// `off`, `ok`, `not_open`, `failing`, `prune_failed`, `over_cap` or
    /// `behind`. In `failing` a poll was not written. In `prune_failed` and
    /// `over_cap` it was, and the file may pass its cap. In `behind` a poll
    /// met a locked file and is held for the next one.
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
    /// The age of each node's last 200. `None` when history is off or the
    /// file could not be read, and `Some` with no entries when no node has
    /// answered 200 in the history yet.
    pub last_ok: Option<Vec<LastOk>>,
}

/// The history store, owned by the loop that writes it.
///
/// One poll adds one batch and writes every held batch that will go. A
/// transaction that meets another connection leaves its batch, and the ones
/// behind it, for the next poll, so the loop never waits on the file. `record`
/// is the only write path and it runs once per poll interval.
pub struct History {
    cfg: Option<Config>,
    store: Option<Store>,
    status: Status,
    /// Batches held for a locked file, oldest first, under the
    /// `history_queue` limit.
    queue: m6_core::limits::Bounded<Batch>,
    /// Polls refused because the hold was past its maximum.
    dropped: u64,
}

impl History {
    /// The file the two endpoints read, or `None` when history is off.
    pub fn file(&self) -> Option<&Path> {
        self.cfg.as_ref().map(|c| c.file.as_path())
    }

    /// The history as the digest and the check report it.
    pub fn digest(&self) -> HistoryDigest {
        let s = &self.status;
        let state = if self.cfg.is_none() {
            "off"
        } else if s.open_error.is_some() {
            "not_open"
        } else if s.last_error.is_some() {
            "failing"
        } else if s.prune_error.is_some() {
            "prune_failed"
        } else if s.over_cap {
            "over_cap"
        } else if !self.queue.is_empty() {
            "behind"
        } else {
            "ok"
        };
        let at = |ms: Option<i64>| ms.map(|m| rfc3339_utc_ms(m).to_string());
        HistoryDigest {
            state,
            file: self.cfg.as_ref().map(|c| c.file.display().to_string()),
            max_bytes: self.cfg.as_ref().map(|c| c.max_bytes),
            file_bytes: s.file_bytes,
            used_bytes: s.used_bytes,
            oldest_at: at(s.oldest_ms),
            newest_at: at(s.newest_ms),
            last_write_age_s: s.last_write.map(|t| t.elapsed().as_secs()),
            writes: s.writes,
            crawl_windows: s.crawl_windows,
            failures: s.failures,
            dropped: self.dropped,
            pruned: s.pruned,
            last_error: s
                .open_error
                .clone()
                .or_else(|| s.last_error.clone())
                .or_else(|| s.prune_error.clone())
                .or_else(|| s.over_cap.then(|| OVER_CAP.to_string())),
            last_ok: self.last_ok(),
        }
    }

    /// The age of each node's last 200, read from the store.
    ///
    /// The history is the record of what the nodes answered, so the age comes
    /// from the rows rather than from a counter the writer keeps: a row that
    /// was pruned stops being a last 200, and the absence is then reported.
    fn last_ok(&self) -> Option<Vec<LastOk>> {
        let at = self.store.as_ref()?.last_ok_at().ok()?;
        let now = now_ms();
        let mut out: Vec<LastOk> = at
            .into_iter()
            .map(|(node, at_ms)| LastOk {
                node,
                age_s: u64::try_from(now.saturating_sub(at_ms)).unwrap_or(0) / 1000,
            })
            .collect();
        out.sort_by(|a, b| a.node.cmp(&b.node));
        Some(out)
    }

    /// Add one poll and write every held batch that will go.
    ///
    /// The hold grows by doubling while the file stays locked. Past the
    /// `history_queue` maximum the new poll is refused with an error that names
    /// the key, and counted, so what is held stays whole and in order.
    pub fn record(&mut self, b: Batch) {
        if self.cfg.is_none() {
            return;
        }
        if let Err((_, refusal)) = self.queue.push_back(b, "a poll") {
            self.dropped += 1;
            m6_core::log::error!("{refusal}");
        }
        self.write_held();
    }

    /// Stop: one last attempt at what is held, then say what is left.
    pub fn flush(&mut self) {
        self.write_held();
        if !self.queue.is_empty() {
            m6_core::log::warn!(
                held = self.queue.len(),
                "history batches were still held when the monitor stopped"
            );
        }
    }

    /// Write the held batches front to back until one meets a lock or fails.
    fn write_held(&mut self) {
        while !self.queue.is_empty() {
            if self.store.is_none() {
                let Some(cfg) = self.cfg.clone() else {
                    self.queue.clear();
                    return;
                };
                match Store::open(&cfg) {
                    Ok(s) => {
                        self.store = Some(s);
                        if self.status.open_error.take().is_some() {
                            m6_core::log::info!(file = %cfg.file.display(), "history file opened");
                        }
                    }
                    Err(e) => {
                        self.queue.pop_front();
                        self.fail(format!("{e:#}"), true);
                        return;
                    }
                }
            }
            let b = self.queue.front().expect("the queue is not empty");
            let result = self.store.as_mut().expect("opened above").write(b);
            let w = match result {
                Ok(w) => w,
                Err(e) if is_busy(&e) => return,
                Err(e) => {
                    self.queue.pop_front();
                    self.store = None;
                    self.fail(format!("{e:#}"), false);
                    return;
                }
            };
            self.queue.pop_front();
            let st = &mut self.status;
            if st.last_error.take().is_some() {
                m6_core::log::info!("history writes recovered");
            }
            st.writes += 1;
            st.crawl_windows += w.crawl_windows;
            st.pruned += w.pruned;
            st.last_write = Some(m6_core::platform::now());
            st.used_bytes = Some(w.used_bytes);
            st.file_bytes = Some(w.file_bytes);
            st.oldest_ms = w.oldest_ms;
            st.newest_ms = w.newest_ms;
            if w.prune_error.is_some() && w.prune_error != st.prune_error {
                m6_core::log::error!(error = ?w.prune_error, "history could not prune");
            }
            if w.cap_unmet && !st.over_cap {
                m6_core::log::error!("{OVER_CAP}");
            }
            st.prune_error = w.prune_error;
            st.over_cap = w.cap_unmet;
        }
    }

    fn fail(&mut self, msg: String, open: bool) {
        let st = &mut self.status;
        st.failures += 1;
        let slot = if open {
            &mut st.open_error
        } else {
            &mut st.last_error
        };
        if slot.as_deref() != Some(msg.as_str()) {
            m6_core::log::error!(error = %msg, "history is not being written");
        }
        *slot = Some(msg);
    }
}

const OVER_CAP: &str =
    "the history is over its cap and deleting every earlier poll cannot get under it";

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
        "prune_failed" => format!(
            "the history could not delete old polls, so the file may grow past its cap: {err}"
        ),
        "over_cap" => format!(
            "the history file is over its cap of {} MB even with every earlier poll deleted, \
             so history_max_mb is too small",
            h.max_bytes.unwrap_or(0) >> 20
        ),
        "behind" => format!(
            "the history is behind and has dropped {} poll(s) since start: \
             is the disk slow or the file locked?",
            h.dropped
        ),
        _ => return None,
    };
    Some(Finding {
        level: Level::Warn,
        node: "monitor".to_string(),
        text,
    })
}

/// Whether a write failed because another connection holds the file.
///
/// This is the one failure that is not an error: it means try again next poll.
/// `SQLITE_BUSY` is a lock and `SQLITE_LOCKED` a table lock in this connection's
/// own file, and both leave the transaction uncommitted.
fn is_busy(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|cause| match cause.downcast_ref::<rusqlite::Error>() {
            Some(rusqlite::Error::SqliteFailure(f, _)) => matches!(
                f.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ),
            _ => false,
        })
}

/// Open the file as the loop's own store.
///
/// A fatal error (a file from a newer release, a bad migration list) is `Err`
/// and the service exits 2. Any other failure leaves the store closed and state
/// `not_open`, and the next poll tries again. The connection's busy timeout is
/// set to zero after the open, so a write that meets a lock returns at once and
/// its batch stays held.
pub fn start(cfg: Option<Config>, queue: &m6_core::limits::Limit) -> anyhow::Result<History> {
    let mut history = History {
        cfg: cfg.clone(),
        store: None,
        status: Status::default(),
        queue: m6_core::limits::Bounded::new(queue),
        dropped: 0,
    };
    let Some(cfg) = cfg else {
        return Ok(history);
    };
    match Store::open(&cfg) {
        Ok(s) => history.store = Some(s),
        Err(e) if is_fatal(&e) => {
            return Err(e.context(format!("history file {}", cfg.file.display())))
        }
        Err(e) => {
            let msg = format!("{e:#}");
            m6_core::log::error!(error = %msg, "history is not being written");
            history.status.open_error = Some(msg);
        }
    }
    Ok(history)
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
    pub net: Vec<NetRow>,
    pub channels: Vec<ChannelOut>,
    pub findings: Vec<FindingOut>,
    pub other_findings: Vec<FindingOut>,
    #[serde(skip)]
    poll_id: i64,
}

const HEALTH_SELECT: &str =
    "SELECT h.id, h.at_ms, n.name, h.level, p.level, h.reachable, h.status, \
h.detail, h.traffic_error, h.rtt_ms, b.binary, b.version, h.uptime_s, h.host_uptime_s, \
h.requests_total, h.monitor_requests, h.cache_hits, h.cache_misses, h.backend_errors, \
h.hit_samples, h.hit_p50_ns, h.hit_p99_ns, h.miss_samples, h.miss_p50_ns, h.miss_p99_ns, \
h.load_one, h.cpus, h.memory_used, h.memory_total_bytes, h.disk_used, h.disk_total_bytes, \
h.thermal_max_c, h.cert_not_after_unix, h.tunnel_rtt_ms, h.tunnel_rtt_n, h.tunnel_probe, \
h.tunnel_probe_error, h.tunnel_handshake, h.tunnel_handshake_age_s, h.tunnel_handshake_detail, \
h.poll_id \
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

const CHANNELS_SELECT: &str = "SELECT hc.health_id, c.label, hc.requests, hc.hits, hc.misses, \
hc.backend_errors, hc.hit_samples, hc.hit_p50_ns, hc.hit_p99_ns, hc.miss_samples, hc.miss_p50_ns, \
hc.miss_p99_ns FROM health_channel hc JOIN channel c ON c.id = hc.channel_id";
const CHANNELS_ORDER: &str = "ORDER BY hc.health_id, c.label";
const NET_SELECT: &str = "SELECT hn.health_id, i.name, hn.rx_bytes, hn.rx_errs, hn.rx_drop, \
hn.tx_bytes, hn.tx_errs, hn.tx_drop FROM health_net hn JOIN iface i ON i.id = hn.iface_id";
const NET_ORDER: &str = "ORDER BY hn.health_id, i.name";
const FINDINGS_SELECT: &str = "SELECT poll_id, level, polled, node, text FROM poll_finding";
const FINDINGS_ORDER: &str = "ORDER BY poll_id, level DESC, id";
const STATUS_SELECT: &str = "SELECT window_id, code, requests FROM crawl_status";
const SIGHTINGS_SELECT: &str =
    "SELECT s.window_id, a.user_agent, s.requests, s.client_ips, s.paths \
FROM crawl_sighting s JOIN agent a ON a.id = s.agent_id";
const SIGHTINGS_ORDER: &str = "ORDER BY s.window_id, s.requests DESC, a.user_agent";

/// A child query for the rows whose `column` is one of `n` ids, which the
/// caller binds in order. A chunk holds at most `CHUNK_ROWS + 1` ids, so the
/// list is short and each id is one probe of the index.
fn children_sql(select: &str, column: &str, n: usize, order: &str) -> String {
    let mut sql = String::with_capacity(select.len() + column.len() + order.len() + 2 * n + 16);
    sql.push_str(select);
    sql.push_str(" WHERE ");
    sql.push_str(column);
    sql.push_str(" IN (");
    for k in 0..n {
        sql.push_str(if k == 0 { "?" } else { ",?" });
    }
    sql.push_str(") ");
    sql.push_str(order);
    sql
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
            let hits: Option<i64> = r.get(16)?;
            let misses: Option<i64> = r.get(17)?;
            let cert: Option<i64> = r.get(32)?;
            let tunnel_probe: Option<String> = r.get(35)?;
            let tunnel = match tunnel_probe {
                None => None,
                Some(probe) => Some(TunnelOut {
                    rtt_ms: r.get(33)?,
                    rtt_n: r.get(34)?,
                    probe,
                    probe_error: r.get(36)?,
                    handshake: r.get(37)?,
                    handshake_age_s: r.get(38)?,
                    handshake_detail: r.get(39)?,
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
                uptime_s: r.get(12)?,
                host_uptime_s: r.get(13)?,
                requests_total: r.get(14)?,
                monitor_requests: r.get(15)?,
                cache_hits: hits,
                cache_misses: misses,
                hit_rate: rate(hits, misses),
                backend_errors: r.get(18)?,
                hit_samples: r.get(19)?,
                hit_p50_ns: r.get(20)?,
                hit_p99_ns: r.get(21)?,
                miss_samples: r.get(22)?,
                miss_p50_ns: r.get(23)?,
                miss_p99_ns: r.get(24)?,
                load_one: r.get(25)?,
                cpus: r.get(26)?,
                memory_used: r.get(27)?,
                memory_total_bytes: r.get(28)?,
                disk_used: r.get(29)?,
                disk_total_bytes: r.get(30)?,
                thermal_max_c: r.get(31)?,
                cert_not_after_unix: cert,
                cert_expires_in_s: cert.map(|c| c - at / 1000),
                tunnel,
                net: Vec::new(),
                channels: Vec::new(),
                findings: Vec::new(),
                other_findings: Vec::new(),
                poll_id: r.get(40)?,
            })
        })?;
        mapped.collect::<rusqlite::Result<_>>()?
    };
    after_rows();
    if out.is_empty() {
        return Ok(out);
    }
    let ids: Vec<i64> = out.iter().map(|r| r.id).collect();
    let index: HashMap<i64, usize> = out.iter().enumerate().map(|(k, r)| (r.id, k)).collect();
    {
        let mut stmt = conn.prepare(&children_sql(
            CHANNELS_SELECT,
            "hc.health_id",
            ids.len(),
            CHANNELS_ORDER,
        ))?;
        let mut rows = stmt.query(rusqlite::params_from_iter(&ids))?;
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
        let mut stmt = conn.prepare(&children_sql(
            NET_SELECT,
            "hn.health_id",
            ids.len(),
            NET_ORDER,
        ))?;
        let mut rows = stmt.query(rusqlite::params_from_iter(&ids))?;
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
    let mut poll_ids: Vec<i64> = out.iter().map(|r| r.poll_id).collect();
    poll_ids.sort_unstable();
    poll_ids.dedup();
    let mut by_poll: HashMap<i64, Vec<(i64, bool, String, String)>> = HashMap::new();
    {
        let mut stmt = conn.prepare(&children_sql(
            FINDINGS_SELECT,
            "poll_id",
            poll_ids.len(),
            FINDINGS_ORDER,
        ))?;
        let mut rows = stmt.query(rusqlite::params_from_iter(&poll_ids))?;
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
    if out.is_empty() {
        return Ok(out);
    }
    let ids: Vec<i64> = out.iter().map(|r| r.id).collect();
    let index: HashMap<i64, usize> = out.iter().enumerate().map(|(k, r)| (r.id, k)).collect();
    {
        let mut stmt = conn.prepare(&children_sql(STATUS_SELECT, "window_id", ids.len(), ""))?;
        let mut rows = stmt.query(rusqlite::params_from_iter(&ids))?;
        while let Some(r) = rows.next()? {
            if let Some(&k) = index.get(&r.get::<_, i64>(0)?) {
                out[k].status.insert(r.get(1)?, r.get(2)?);
            }
        }
    }
    {
        let mut stmt = conn.prepare(&children_sql(
            SIGHTINGS_SELECT,
            "s.window_id",
            ids.len(),
            SIGHTINGS_ORDER,
        ))?;
        let mut rows = stmt.query(rusqlite::params_from_iter(&ids))?;
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
    use m6_core::monitoring::{LoggingHealth, TrafficReport};
    use m6_core::platform::host::{
        Disk, HostSnapshot, LoadAverage, Memory, MemorySource, NetDevice,
    };
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
            events_dropped: 0,
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
            first_seen: "2026-10-01T09:00:00Z".into(),
            last_seen: "2026-10-01T09:05:00Z".into(),
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
            thermal: vec![m6_core::platform::host::ThermalZone {
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
            health: Some(m6_core::monitoring::HealthReport {
                process_index: None,
                status: "ok".into(),
                node: name.into(),
            }),
            health_status: Some(200),
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
        assert_eq!(sqlite::schema_version(&s.conn).unwrap(), 3);
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
        assert_eq!(sqlite::schema_version(&s.conn).unwrap(), 3);
    }

    /// The last-200 read is answered from the partial index. The plan must name
    /// it and must not scan the health table, which grows with the history.
    #[test]
    fn the_last_ok_read_uses_the_partial_index_and_no_table_scan() {
        let dir = tempfile::tempdir().unwrap();
        let s = open(dir.path());
        let mut stmt = s
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {LAST_OK_SQL}"))
            .unwrap();
        let plan: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let plan = plan.join("\n");
        assert!(plan.contains("health_ok_node_at"), "{plan}");
        assert!(
            !plan
                .lines()
                .any(|l| l.contains("SCAN h") || l.contains("SCAN health")),
            "{plan}"
        );
    }

    /// A file written by migrations 1 and 2 opens, takes migration 3, and keeps
    /// every row and every other column. Two builds that differed only in
    /// `hash` become one, and the health row that named the second names the
    /// first.
    #[test]
    fn a_file_from_schema_2_drops_the_build_hash_and_keeps_the_rest() {
        type Rows = Vec<Vec<rusqlite::types::Value>>;
        fn rows(c: &Connection, sql: &str) -> Rows {
            let mut stmt = c.prepare(sql).unwrap();
            let n = stmt.column_count();
            stmt.query_map([], |r| (0..n).map(|i| r.get(i)).collect())
                .unwrap()
                .map(Result::unwrap)
                .collect()
        }
        fn columns(c: &Connection, table: &str) -> Vec<String> {
            let mut stmt = c
                .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        }

        let dir = tempfile::tempdir().unwrap();
        let file = cfg(dir.path(), 1).file;
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        {
            let mut c = Connection::open(&file).unwrap();
            sqlite::migrate(&mut c, &MIGRATIONS[..2]).unwrap();
            c.execute_batch(
                "INSERT INTO node (id, name) VALUES (1, 'origin');
                 INSERT INTO build (id, binary, version, hash) VALUES
                   (1, 'm6-http', '1.15.0', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'),
                   (2, 'm6-http', '1.15.0', 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'),
                   (3, 'm6-http', '1.16.0-rc.1+abc123def456', 'abc123def456');
                 INSERT INTO poll (id, at_ms, level, faults, warns) VALUES (1, 1000, 1, 0, 1);
                 INSERT INTO poll_finding (id, poll_id, level, node, polled, text)
                   VALUES (1, 1, 1, 'origin', 1, 'a finding');
                 INSERT INTO health (id, poll_id, at_ms, node_id, level, reachable, status,
                   detail, rtt_ms, build_id, uptime_s, requests_total, load_one, tunnel_probe)
                 VALUES
                   (1, 1, 1000, 1, 0, 1, 'ok', NULL, 1.5, 1, 10, 100, 0.25, 'ok'),
                   (2, 1, 1001, 1, 1, 1, 'ok', 'slow', 2.5, 2, 11, 101, 0.5, NULL),
                   (3, 1, 1002, 1, 2, 0, 'down', 'refused', NULL, 3, NULL, NULL, NULL, NULL),
                   (4, 1, 1003, 1, 0, 0, 'down', NULL, NULL, NULL, NULL, NULL, NULL, NULL);",
            )
            .unwrap();
        }
        let before = {
            let c = Connection::open(&file).unwrap();
            assert_eq!(sqlite::schema_version(&c).unwrap(), 2);
            (
                rows(&c, "SELECT * FROM health ORDER BY id"),
                rows(&c, "SELECT * FROM poll ORDER BY id"),
                rows(&c, "SELECT * FROM poll_finding ORDER BY id"),
                rows(&c, "SELECT * FROM node ORDER BY id"),
            )
        };

        let s = open(dir.path());
        assert_eq!(sqlite::schema_version(&s.conn).unwrap(), 3);
        assert_eq!(columns(&s.conn, "build"), ["id", "binary", "version"]);
        assert_eq!(
            rows(&s.conn, "SELECT id, binary, version FROM build ORDER BY id"),
            rows(
                &s.conn,
                "SELECT 1, 'm6-http', '1.15.0' UNION ALL SELECT 3, 'm6-http', '1.16.0-rc.1+abc123def456'"
            ),
            "every build keeps its id, binary and version, and the two that differed only in hash are one"
        );

        let mut health = before.0.clone();
        // Health row 2 named build 2, which is now build 1.
        let build_id = columns(&s.conn, "health")
            .iter()
            .position(|c| c == "build_id")
            .expect("health has build_id");
        health[1][build_id] = rusqlite::types::Value::Integer(1);
        assert_eq!(rows(&s.conn, "SELECT * FROM health ORDER BY id"), health);
        assert_eq!(rows(&s.conn, "SELECT * FROM poll ORDER BY id"), before.1);
        assert_eq!(
            rows(&s.conn, "SELECT * FROM poll_finding ORDER BY id"),
            before.2
        );
        assert_eq!(rows(&s.conn, "SELECT * FROM node ORDER BY id"), before.3);

        assert!(
            sqlite::is_append_only(&s.conn, "build").unwrap(),
            "the rebuilt table keeps the lookup-table rule"
        );
        let broken: i64 = s
            .conn
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(broken, 0, "every health row names a build that exists");
    }

    #[test]
    fn a_file_newer_than_the_code_is_fatal() {
        let dir = tempfile::tempdir().unwrap();
        drop(open(dir.path()));
        let c = Connection::open(cfg(dir.path(), 1).file).unwrap();
        c.pragma_update(None, "user_version", 4).unwrap();
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
    fn tunnel_figures_are_stored() {
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
        let (rtt, n, probe, hs, age): (f64, i64, String, String, i64) = s
            .conn
            .query_row(
                "SELECT tunnel_rtt_ms, tunnel_rtt_n, tunnel_probe, tunnel_handshake, \
                 tunnel_handshake_age_s FROM health",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(
            (n, probe.as_str(), hs.as_str(), age),
            (3, "connected", "ok", 42)
        );
        assert!((rtt - 0.4).abs() < 1e-9);
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
            // The same release tag, rebuilt from a different commit. The build
            // row is keyed on the commit in the version's build metadata, so
            // this must be a second row.
            p.build.version = "1.4.0+ffffffffffff".into();
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
        // The assertion below is this cap plus one SQLite page, so the two
        // move together. A 1 MB cap with a 2 MB bound let a 1.9 MB file pass a
        // test whose name says the file stays under its cap.
        let mut s = Store::open(&cfg(dir.path(), 2)).unwrap();
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
            file_len(dir.path()) <= (2 << 20) + 65_536,
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
                 rtt_ms binary version uptime_s host_uptime_s requests_total monitor_requests \
                 cache_hits cache_misses hit_rate backend_errors hit_samples hit_p50_ns hit_p99_ns \
                 miss_samples miss_p50_ns miss_p99_ns load_one cpus memory_used memory_total_bytes \
                 disk_used disk_total_bytes thermal_max_c cert_not_after_unix cert_expires_in_s \
                 tunnel net channels findings other_findings"
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

    /// A write that meets another connection's lock returns at once and its
    /// batch is kept. The next poll writes the held batch and its own.
    #[test]
    fn a_busy_history_database_keeps_the_batch() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = start(Some(cfg(dir.path(), 64))).unwrap();
        let lock = Connection::open(&cfg(dir.path(), 64).file).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();

        let started = m6_core::platform::now();
        h.record(make(T0, &[reading("a")]));
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "record waited on the lock: {:?}",
            started.elapsed()
        );
        let d = h.digest();
        assert_eq!(d.writes, 0);
        assert_eq!(d.state, "behind");

        lock.execute_batch("COMMIT").unwrap();
        h.record(make(T0 + 1, &[reading("a")]));
        let d = h.digest();
        assert_eq!(d.writes, 2, "the held batch and the new one");
        assert_eq!(d.state, "ok");
    }

    /// A hold past the `history_queue` maximum refuses the new poll with an
    /// error naming the key and counts it, so what is held stays whole and in
    /// order.
    #[test]
    fn a_hold_past_its_maximum_refuses_the_new_poll_and_counts_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = super::start(Some(cfg(dir.path(), 64)), &queue_limit(2, QUEUE_POLLS)).unwrap();
        let lock = Connection::open(&cfg(dir.path(), 64).file).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        for i in 0..QUEUE_POLLS as i64 + 2 {
            h.record(make(T0 + i, &[reading("a")]));
        }
        let d = h.digest();
        assert_eq!(d.dropped, 2);
        assert_eq!(d.writes, 0);

        lock.execute_batch("COMMIT").unwrap();
        h.flush();
        assert_eq!(h.digest().writes, QUEUE_POLLS as u64);
        let rows: Vec<i64> = h
            .store
            .as_ref()
            .unwrap()
            .conn
            .prepare("SELECT at_ms FROM poll ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let want: Vec<i64> = (0..QUEUE_POLLS as i64).map(|i| T0 + i).collect();
        assert_eq!(
            rows, want,
            "the first {QUEUE_POLLS} polls are kept in order"
        );
    }

    /// The most polls a test's hold takes.
    const QUEUE_POLLS: usize = 8;

    /// `start` with the hold's default limit.
    fn start(cfg: Option<Config>) -> anyhow::Result<History> {
        super::start(cfg, &queue_limit(2, QUEUE_POLLS))
    }

    /// The hold's limit for a test.
    fn queue_limit(start: usize, max: usize) -> m6_core::limits::Limit {
        m6_core::limits::Limit::custom(&m6_core::limits::HISTORY_QUEUE, start, max)
    }

    fn history(c: Config) -> History {
        History {
            cfg: Some(c),
            store: None,
            status: Status::default(),
            queue: m6_core::limits::Bounded::new(&queue_limit(2, QUEUE_POLLS)),
            dropped: 0,
        }
    }

    #[test]
    fn a_failed_open_is_reported_and_the_next_poll_retries() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("sub");
        std::fs::write(&blocker, b"a file where the directory should be").unwrap();
        let mut h = history(cfg(dir.path(), 1));
        let b = make(T0, &[reading("a")]);
        h.record(b.clone());
        let d = h.digest();
        assert_eq!(d.state, "not_open");
        assert_eq!(d.failures, 1);
        assert!(finding(&d).is_some());
        std::fs::remove_file(&blocker).unwrap();
        h.record(b);
        let d = h.digest();
        assert_eq!((d.state, d.writes), ("ok", 1));
        assert_eq!(d.last_error, None);
        assert!(finding(&d).is_none());
    }

    #[test]
    fn a_failed_write_is_failing_until_one_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = history(cfg(dir.path(), 1));
        h.record(make(T0, &[reading("a")]));
        let mut bad = make(T0 + 1, &[reading("a")]);
        let dup = bad.rows[0].channels[0].clone();
        bad.rows[0].channels.push(dup);
        h.record(bad);
        assert_eq!(h.digest().state, "failing");
        h.record(make(T0 + 2, &[reading("a")]));
        let d = h.digest();
        assert_eq!((d.state, d.writes, d.failures), ("ok", 2, 1));
    }

    /// The age of each node's last 200 is read from the rows, and a restarted
    /// monitor reads the same answer out of the file.
    #[test]
    fn the_age_of_the_last_200_comes_from_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = history(cfg(dir.path(), 64));
        let mut r = reading("origin");
        r.health = Some(m6_core::monitoring::HealthReport {
            process_index: None,
            status: "ok".into(),
            node: "origin".into(),
        });
        r.health_status = Some(200);
        h.record(make(now_ms() - 300_000, std::slice::from_ref(&r)));
        let d = h.digest();
        let last_ok = d.last_ok.clone().expect("the history was read");
        assert_eq!(last_ok.len(), 1, "{last_ok:?}");
        assert_eq!(last_ok[0].node, "origin");
        assert!(
            (300..=301).contains(&last_ok[0].age_s),
            "300 seconds ago reads as {:?}",
            last_ok[0]
        );
        // A new process asks the file the same question.
        let reopened = start(Some(cfg(dir.path(), 64))).unwrap();
        let again = reopened.digest().last_ok.expect("the history was read");
        assert_eq!(again.len(), 1, "{again:?}");
        assert_eq!(again[0].node, "origin");
        assert!((300..=301).contains(&again[0].age_s), "{again:?}");
    }

    /// A degraded poll is not a 200, so the age keeps running from the last
    /// answer that was.
    #[test]
    fn a_degraded_poll_does_not_reset_the_age_of_the_last_200() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = history(cfg(dir.path(), 64));
        let mut r = reading("origin");
        r.health = Some(m6_core::monitoring::HealthReport {
            process_index: None,
            status: "ok".into(),
            node: "origin".into(),
        });
        r.health_status = Some(200);
        h.record(make(now_ms() - 300_000, std::slice::from_ref(&r)));
        r.health = Some(m6_core::monitoring::HealthReport {
            process_index: None,
            status: "degraded".into(),
            node: "origin".into(),
        });
        r.health_status = Some(503);
        h.record(make(now_ms() - 1000, std::slice::from_ref(&r)));
        let last_ok = h.digest().last_ok.expect("the history was read");
        assert_eq!(last_ok.len(), 1, "{last_ok:?}");
        assert!((300..=301).contains(&last_ok[0].age_s), "{last_ok:?}");
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
        let off = start(None).unwrap();
        assert_eq!(off.digest().state, "off");
        assert!(finding(&off.digest()).is_none());

        let dir = tempfile::tempdir().unwrap();
        let mut h = start(Some(cfg(dir.path(), 1))).unwrap();
        let path = cfg(dir.path(), 1).file.display().to_string();
        assert_eq!(h.digest().state, "ok");
        assert!(finding(&h.digest()).is_none());

        h.status.open_error = Some("denied".into());
        assert_eq!(
            finding(&h.digest()).map(|f| (f.level, f.node, f.text)),
            Some((
                Level::Warn,
                "monitor".into(),
                format!(
                    "history file {path} could not be opened, so no history is being kept: denied"
                )
            ))
        );
        h.status.open_error = None;
        h.status.last_error = Some("disk full".into());
        assert_eq!(
            finding(&h.digest()).unwrap().text,
            "the last history write failed, so that poll is missing from the history: disk full"
        );
        h.status.last_error = None;
        h.dropped = 2;
        let held = h.queue.push_back(make(T0, &[reading("a")]), "a poll");
        assert!(held.is_ok(), "the hold has room");
        assert_eq!(h.digest().state, "behind");
        assert_eq!(
            finding(&h.digest()).unwrap().text,
            "the history is behind and has dropped 2 poll(s) since start: is the disk slow or the file locked?"
        );
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
        // A 2 MB cap, not 8 MB. The property is that the cap is reached and the
        // unused agent rows are pruned, and it needs enough windows for the
        // file to overflow whatever cap is set. 8 MB meant 1500 windows and
        // 30,000 inserts, 10.8 s for one unit test on the build host. The same
        // overflow arrives at 2 MB with 350 windows, which is still many
        // windows, and it still leaves more than the 100-poll floor below.
        let mut s = Store::open(&cfg(dir.path(), 2)).unwrap();
        let mut pruned = 0;
        let windows = 350i64;
        for k in 0..windows {
            let crawlers = (0..10u64)
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
            file_len(dir.path()) <= (2 << 20) + 65_536,
            "{}",
            file_len(dir.path())
        );
        let (polls, newest): (i64, i64) = s
            .conn
            .query_row("SELECT count(*), max(at_ms) FROM poll", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        // A 2 MB cap retains about 94 polls at this window size, so the floor
        // is the same "a useful history survives" check scaled to the smaller
        // cap. It is above zero by a wide margin, so a prune that emptied the
        // history still fails.
        assert!(polls >= 90, "{polls}");
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

    /// The plan of a child query, one line per step.
    fn plan(conn: &Connection, sql: &str, n: usize) -> String {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let ids: Vec<i64> = (1..=n as i64).collect();
        let rows = stmt
            .query_map(rusqlite::params_from_iter(&ids), |r| r.get::<_, String>(3))
            .unwrap();
        rows.map(Result::unwrap).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn child_queries_probe_the_exact_ids_of_the_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let s = open(dir.path());
        for (select, col, order, probe) in [
            (
                CHANNELS_SELECT,
                "hc.health_id",
                CHANNELS_ORDER,
                "health_id=?",
            ),
            (NET_SELECT, "hn.health_id", NET_ORDER, "health_id=?"),
            (FINDINGS_SELECT, "poll_id", FINDINGS_ORDER, "poll_id=?"),
            (STATUS_SELECT, "window_id", "", "window_id=?"),
            (
                SIGHTINGS_SELECT,
                "s.window_id",
                SIGHTINGS_ORDER,
                "window_id=?",
            ),
        ] {
            let sql = children_sql(select, col, 3, order);
            assert_eq!(sql.matches('?').count(), 3, "{sql}");
            let p = plan(&s.conn, &sql, 3);
            assert!(p.contains(probe), "{col}: {p}");
            assert!(!p.contains(">?"), "{col} reads a range: {p}");
        }
    }

    #[test]
    fn a_node_page_returns_only_that_nodes_children() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(dir.path());
        for k in 0..5 {
            let rs = [reading("a"), unreachable("b"), reading("c")];
            let d = digest::build(&rs, &Thresholds::default(), "t".into());
            s.write(&batch(T0 + k * 1000, &rs, &d)).unwrap();
        }
        let b = node_id(&s.conn, "b").unwrap().unwrap();
        let rows = health_page(&s.conn, T0, T0 + 10_000, Some(b), None, 100).unwrap();
        assert_eq!(rows.len(), 5);
        assert!(rows.iter().all(|r| r.node == "b"));
        assert!(rows
            .iter()
            .all(|r| r.channels.is_empty() && r.net.is_empty()));
        assert!(rows.iter().all(|r| r.findings.len() == 1));
        let c = node_id(&s.conn, "c").unwrap().unwrap();
        let rows = health_page(&s.conn, T0, T0 + 10_000, Some(c), None, 100).unwrap();
        assert!(rows
            .iter()
            .all(|r| r.channels.len() == 2 && r.net.len() == 1));
    }

    /// A config whose cap leaves room for `spare` bytes over one poll.
    fn tight(dir: &Path, spare: u64) -> Config {
        let probe = tempfile::tempdir().unwrap();
        let mut s = open(probe.path());
        let w = s.write(&make(T0, &[reading("a")])).unwrap();
        Config {
            max_bytes: w.used_bytes + spare,
            ..cfg(dir, 1)
        }
    }

    #[test]
    fn a_failed_prune_is_not_a_lost_poll() {
        let dir = tempfile::tempdir().unwrap();
        let c = tight(dir.path(), 20_000);
        let mut h = start(Some(c.clone())).unwrap();
        h.record(make(T0, &[reading("a")]));
        let blocker = sqlite::open(&c.file).unwrap();
        blocker
            .execute_batch(
                "CREATE TRIGGER keep_polls BEFORE DELETE ON poll \
                 BEGIN SELECT RAISE(ABORT, 'polls are kept'); END",
            )
            .unwrap();
        let mut writes = 1;
        for k in 1..200 {
            h.record(make(T0 + k * 1000, &[reading("a")]));
            writes += 1;
            if h.digest().state != "ok" {
                break;
            }
        }
        let d = h.digest();
        assert_eq!(d.state, "prune_failed");
        assert_eq!((d.writes, d.failures), (writes, 0), "the poll was written");
        assert!(d.last_error.unwrap().contains("polls are kept"));
        let f = finding(&h.digest()).unwrap().text;
        assert!(f.contains("could not delete old polls"), "{f}");
        assert!(!f.contains("missing"), "{f}");
        let polls: i64 = blocker
            .query_row("SELECT count(*) FROM poll", [], |r| r.get(0))
            .unwrap();
        assert_eq!(polls, writes as i64);
        blocker.execute_batch("DROP TRIGGER keep_polls").unwrap();
        h.record(make(T0 + 1_000_000, &[reading("a")]));
        let d = h.digest();
        assert_eq!(d.state, "ok", "{d:?}");
        assert!(d.pruned > 0);
        assert_eq!(d.last_error, None);
    }

    #[test]
    fn a_cap_that_cannot_be_met_is_over_cap_and_keeps_the_poll() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = start(Some(Config {
            max_bytes: 4096,
            ..cfg(dir.path(), 1)
        }))
        .unwrap();
        h.record(make(T0, &[reading("a")]));
        let d = h.digest();
        assert_eq!((d.state, d.writes, d.failures), ("over_cap", 1, 0));
        let f = finding(&d).unwrap();
        assert_eq!(f.level, Level::Warn);
        assert!(f.text.contains("over its cap"), "{}", f.text);
        assert!(!f.text.contains("missing"), "{}", f.text);
        assert!(d.last_error.unwrap().contains("over its cap"));
        // Only the poll just written is left, and it is that one.
        let c = rusqlite::Connection::open(dir.path().join("sub").join("history.sqlite3")).unwrap();
        let rows: Vec<i64> = c
            .prepare("SELECT at_ms FROM poll")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows, vec![T0]);
    }

    #[test]
    fn a_cap_that_cannot_be_met_keeps_only_the_newest_poll() {
        let dir = tempfile::tempdir().unwrap();
        let c = Config {
            max_bytes: 4096,
            ..cfg(dir.path(), 1)
        };
        let mut s = Store::open(&c).unwrap();
        s.write(&make(T0, &[reading("a")])).unwrap();
        s.write(&make(T0 + 30_000, &[reading("a")])).unwrap();
        let rows: Vec<i64> = s
            .conn
            .prepare("SELECT at_ms FROM poll")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows, vec![T0 + 30_000]);
    }

    fn mode(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_history_directory_and_file_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path(), 1);
        let s = Store::open(&c).unwrap();
        assert_eq!(mode(c.file.parent().unwrap()), 0o700);
        assert_eq!(mode(&c.file), 0o600);
        drop(s);
        // A file from an earlier start is tightened. A directory that was
        // already there is not the monitor's to change.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&c.file, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(
            c.file.parent().unwrap(),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        Store::open(&c).unwrap();
        assert_eq!(mode(&c.file), 0o600);
        assert_eq!(mode(c.file.parent().unwrap()), 0o755);
    }

    #[test]
    fn a_journal_beside_the_history_file_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path(), 1);
        let mut s = Store::open(&c).unwrap();
        s.write(&make(T0, &[reading("a")])).unwrap();
        for entry in std::fs::read_dir(c.file.parent().unwrap()).unwrap() {
            let p = entry.unwrap().path();
            assert_eq!(mode(&p) & 0o077, 0, "{}", p.display());
        }
    }
}
