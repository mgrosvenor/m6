# m6-monitor: the fleet's figures from its journal

m6-monitor reports how a fleet of m6 nodes is doing.
It reads the monitor host's copy of every node's journal with one `journalctl` read at start and one each poll interval, and asks each node for `GET /health` on every poll.
From those two inputs it builds one digest, serves it as a page, as *JSON* and as text, and records every poll in a history file when the config names one.
All five routes answer on a unix socket, and the monitor has no listener of its own.

Sections 1 to 3 describe the two inputs.
Section 4 follows one poll from its first step to the snapshot.
Section 5 gives the configuration, section 6 the routes and section 7 every number the digest carries.
Section 8 covers the history file, and section 9 says where the crate sits.

## Contents

1. [The two inputs](#1-the-two-inputs)
2. [The journal reader](#2-the-journal-reader)
3. [What the log cannot answer](#3-what-the-log-cannot-answer)
4. [One poll](#4-one-poll)
5. [Configuration](#5-configuration)
6. [The routes](#6-the-routes)
7. [The digest and the check](#7-the-digest-and-the-check)
8. [History](#8-history)
9. [Where it fits](#9-where-it-fits)

---

## 1. The two inputs

Every figure the monitor reports is one of two things: a document a node wrote as a log line, or a node's answer to `GET /health`.
Table 1 gives both inputs, what each carries, and how current it is.

| input | read by | what it carries | how current |
|---|---|---|---|
| the journal | `journal::Reader` in `m6-monitor/src/journal.rs` | each node's metrics document and traffic document: host figures, requests, cache, *TLS* chains, crawlers, firewall state | the newest line the journal holds |
| the journal | `journal::Startup` in the same module | one start-up record per run: identity, invocation, a sha256 per config file, every setting with its source, the host facts, the decided facts, then the completeness count, and the shutdown line on a clean exit | the newest record the journal holds, plus every record that never closed |
| `GET /health` | `poll::Step::Health` in `m6-monitor/src/poll.rs` | liveness, the `ok` or `degraded` answer, the node's own name, its process index, and the round trip from this host | once per poll |

**Table 1: the two inputs and what each one carries.**

Each node ships its journal to the monitor host with `systemd-journal-upload`, and `systemd-journal-remote` keeps every peer's journal in one directory, which `[monitor] journal_dir` names.
The two documents arrive as log lines with `type` `perf` and `type` `traffic`.
The record arrives as several lines with `type` `startup` and one with `type` `shutdown`.
`docs/m6-log.md` section 9 gives the record's parts and keys, and `docs/m6-http.md` section 11 gives the two documents.

`GET /health` is the request every node sees on every poll.
A fleet can also declare header checks, and every configured tunnel is probed, so section 4 places both beside the two inputs.

Section 2 follows one read of the journal, and section 3 the one input that is a request.

## 2. The journal reader

The journal is read once at start and once each poll interval.
Each read starts a `journalctl` child that prints the last lines and exits, and its stdout is one more descriptor in the monitor's event loop.

`journal::Reader::open` starts `journalctl` with `--lines=<N>`, `--all`, `--output=json`, `--directory <journal_dir>`, and four matches: `M6_TYPE=perf`, `M6_TYPE=traffic`, `M6_TYPE=startup` and `M6_TYPE=shutdown`.
One child carries all four types.
`M6_TYPE` is the journal field `m6-log` writes for every line under systemd, set from the line's own `type` (`m6-log/src/journal.rs`), so the child selects the types before any JSON is parsed.
`--all` is there because JSON output nulls a field whose name and value reach 4096 bytes together, and a metrics document is about 6.4 KB.
N is the larger of 200 and 14 for each `[[monitor.nodes]]` entry.
A node writes about 7 matching lines a minute, 6 metrics documents at 10 seconds and 1 traffic document at 60, so 14 lines for each node cover at least two minutes of the whole fleet, which is more than two poll intervals at the default 30 seconds.
Start-up lines share the window and shorten it a little, and the floor of 200 keeps a small fleet's window large enough to hold a start-up record.
The count reads no clock of the monitor's, and the newest entries are chosen by the senders' clocks.
The reads overlap, and each entry is taken once.
A read that failed or was killed changes nothing for the next one.
Known limits, for this release: the window is the newest N entries of every node merged and ordered by each sender's own clock, so it spans a period of each sender's time and holds no guaranteed share for each node.
A node whose clock lags by more than the window's span is not read, and a node whose clock runs ahead, or that writes much faster than the others, fills the window and hides the rest.
A poll interval over about two minutes on a fleet of more than 14 nodes is not covered, because the window holds about two minutes of the fleet.
A start-up record older than the window is not read, and its absence is not a finding, so `--check` then skips that node's version, config-sha256, decided and completeness checks.
A read per node is the planned fix.

A read does not follow.
`journalctl --follow` over a remote journal that two hosts upload into returns only one host's entries, whichever it started behind, and never the other's.
The uploaders deliver their batches with different delays, so the lagging host's new entries are older than the follower's position and are never returned.
The same command without `--follow` prints both hosts' entries every time, so the service reads the last lines and exits, the way `--check` always has.

The pipe is made non-blocking and registered under the `JOURNAL` token in `m6-monitor/src/main.rs`.
The loop reads whole lines from it and hands each to `Poller::read_journal`, so the loop never waits on the child.
At most one read starts per poll interval, on a pass of the loop's one-second tick, so a poll that overruns its interval does not start a read on every pass.
A read still in flight is not started again.
A read still in flight one interval after it started is killed, reported in `Reader::stopped`, and the next read starts normally.
The loop never waits for the child: `Reader::retire` reaps it only when it has exited, and a later pass reaps it otherwise.
`journal::Latest` keeps the newest metrics document and the newest traffic document for each node name, and `Poller::record_journal` reads those two maps when a poll reaches its `Perf` and `Traffic` steps.
It orders each node's documents by the entry's `__REALTIME_TIMESTAMP`, which a remote journal stores as the uploading node's own clock, so it orders the entries kept under one node name, which a node chooses in its own document.
Nothing inside `MESSAGE` orders anything, because the node writes all of it.
An entry without the field is counted as malformed, and an older entry read later does not replace a newer one.
The reads overlap, so a start-up line that arrives again is counted once, by its recorded time and `seq`.
The same `ingest` call hands a `startup` or `shutdown` line to `journal::Startup`, which groups the several lines of one record by their shared `run_id`.

A start-up record's node is the record's own `node` field.
A record already in the journal without that field takes the hostname on its own platform line, which is the machine it runs on.
The field holds the app's configured node name when its config has one and the platform facts' hostname otherwise, and the record's `node_source` says which.
Two instances on one host that name themselves differently are two nodes here.
The app is the record's own `app` field.
The key the report shows is the node and the app, and the deploy's expectation file uses the same key (section 5.1).
A record that reports no hostname is attributed to its app alone, and the check warns that a second host running the same app would be merged with it.

A record is judged only when the monitor saw it from its first line.
The read's `--lines` tail can start in the middle of a record, so a record whose first line seen is not `seq` 1 is counted in `Startup::cut` and neither shown nor judged.
A record that started before the monitor did is shown as the current run and is not judged for its completeness or its restarts, because the tail can cut its last lines as easily as its first.
A record still arriving after `journal::INCOMPLETE_AFTER`, 120 seconds, is moved to the report's unfinished list, which is how a start that never finishes becomes a finding.

A `perf` or `traffic` line is attributed by the `node` field of the document inside `MESSAGE`, which is the value the node's own config gives `[node] name`.
A host that runs three nodes writes one `_HOSTNAME` for all three, so the monitor never reads it.
A `perf` or `traffic` line whose document names no node is counted in `Latest::unattributed` and dropped.
An entry that is not JSON, or one whose `MESSAGE` is missing or not JSON, is counted in `Latest::malformed`, and a line of another `type` is ignored.

The fleet entry's `name` is the key the monitor looks a document up by, so it has to equal the node's `[node] name`.
A fleet entry that names a node differently gets no figures for it, and the digest reports `no metrics line in the journal for this node` while `reported_node` shows the name the node calls itself.

A line longer than `[limits] journal_line` is counted in `Reader::oversized` and dropped, and the reader discards bytes to the next newline so the following line starts clean.
When the child exits, the loop deregisters the pipe and `Reader::retire` reaps the process.
`Reader::start_if_idle` starts the next read when an interval has passed, and leaves a read that is still in flight alone.
A reader made by `Reader::unavailable`, for a `journal_dir` that is missing, starts nothing and keeps its reason, which names the key to fix.
`Reader::stopped` keeps the reason a read could not start or did not exit cleanly, and the poller appends it to a node's missing-document error.
At stop, `Reader`'s `Drop` kills and waits, so no `journalctl` outlives the monitor.

`record_journal` reports a node with no document: it sets `no metrics line in the journal for this node`, and `no traffic line in the journal for this node` for the traffic document.
The digest turns the missing metrics document into a Warn finding, `metrics unavailable: ... Health is known, everything else is not.`, which on its own leaves `/check` at exit 0 and the page at 200.
A node that cannot be reached finishes its poll at the `Health` step, so it carries no document error and its `status` is `unreachable`.

The same reader serves the one-shot check: `Reader::once` starts one read, `Reader::drain` reads the pipe to its end, and `m6-monitor --check` reads with the built-in `journal_line` limit.
A relative `journal_dir` resolves against the config file's directory under `--check`, and against the site directory in the service.

Section 3 turns to the one input that is a request.

## 3. What the log cannot answer

A document says nothing about whether the node is answering now, and some questions can be settled only by asking it.

A node that is down, a node whose journal upload has stopped and an idle node all leave `Latest` unchanged, so the last document a node wrote stays in the digest at any age.
`GET /health` is the per-poll fact, and `NodeReading::status` is the single rule that reads it: a 200 carries the body's `status`, any other code is `degraded`, a 200 with no parseable body is `unknown`, and a node that did not answer is `unreachable`.
A 503 `degraded` says the process is alive and serving its public and cached pages while a dependency it reads is failing, which on a cache node is its origin (`origin_down` in `m6-http/src/health.rs`).
The node's log names the failing dependency, and `digest::DEGRADED_REASON` says so in the check's HEALTH line.

The second question is where the node is.
`NodeReading::rtt` is the round trip to `/health` measured from the monitor host, connect and TLS handshake included, so it measures the path from here to the node's public address.

The third is which node answered.
The body carries the node's own name and its process index, and `digest` reports the name as `reported_node` beside the fleet entry's `name`, so a fleet entry pointing at the wrong box is visible.
The process index lets a poll tell the processes of a `processes` entry apart.

The age of a node's last 200 comes from the history.
The history answers it from the partial index `health_ok_node_at`, which holds only the rows that count as a 200, and `check::last_ok` reads that age.
`[monitor] health_last_ok_max_age_s`, three poll intervals by default, is the age above which the HEALTH line reads `FAULT`.
Without a history file the line reads `last 200 unknown (history off)`, and `--check` reads `last 200 unknown (--check keeps no history)`.

A document carries no timestamp of its own, so the digest cannot say how old a node's figures are.
A reading states its uptime and the seconds since its last log line, and the digest's `snapshot_age_s` is the age of the poll.

Section 4 puts both inputs in the order one poll reaches them.

## 4. One poll

A poll is a table of steps the loop drives, one slot per node, with no thread and no blocking call.
Table 2 lists the steps in the order a node reaches them.

| step | what it does | where |
|---|---|---|
| `Health` | `GET {base_path}/health` on the node's public address, or on its status port for an entry expanded from `processes` | `Poller::begin_step` |
| `Perf` | reads the newest metrics document for this node from `Latest` | `Poller::record_journal` |
| `Traffic` | reads the newest traffic document for this node from `Latest` | `Poller::record_journal` |
| `Header(i)` | one declared header check, a `GET` on the node's public address | `Poller::begin_step` |
| the tunnel probe | up to three non-blocking connects to the node's `tunnel.address`, after the node's requests | `tunnel::PROBES_PER_POLL` |
| the crawler lookups | reverse and forward *DNS* lookups for the addresses in a sighting | `check::VerdictCache` |

**Table 2: the steps of one poll, in order.**

Only `Health` and `Header` send a request, so a fleet with no header checks declared makes exactly one request per node per poll.
A header check runs on a node whose `role` its entry lists, and on process 0 of a `processes` node, because the public face is one per node (`Poller::next_header`).
A request is `GET <path> HTTP/1.1` with `Host` and `Connection: close`, and it carries no credential (`write_request`).
Each node's steps advance as its own descriptors fire, so the nodes make progress at once, and a node that hangs holds its own slot until `timeout_ms` passes.
`Poller::expire` records that timeout as the step's network error and moves the node on.

A poll starts on the one-second tick, when `poll_interval_s` has passed since the last one started.
A poll still running at its next due time is not restarted, and `main.rs` names that once in the debug log.

`publish` builds the digest, records the history batch, renders the page once, and stores all of it as the snapshot.
`/` serves those stored bytes, while `/digest` and `/check` build their bodies per request so their age fields are the age at the request.
A poll that does not finish leaves the previous snapshot in place and its age keeps growing.
At three poll intervals the reading is stale: `/digest` sets `stale` to true and `/check` prints a `STALE:` line first.

Section 5 lists the keys that decide all of this.

## 5. Configuration

The monitor reads its own keys under `[monitor]`, its poll interval from the top level, and its buffers from `[limits]`.
Table 3 lists the `[monitor]` keys, their defaults, and what each one decides.

| key | default | what it decides |
|---|---|---|
| `nodes` | required, at least one | the fleet, as `[[monitor.nodes]]` entries |
| `timeout_ms` | 5000 | the deadline for each step of a node's poll |
| `journal_dir` | `/var/log/journal/remote` | the directory each read opens, resolved against the site directory |
| `history_file` | absent | the *SQLite* file that keeps the polls |
| `history_max_mb` | 256, from 1 to 1048576 | the file's cap |
| `one_certificate` | `false` | whether every node is expected to serve one certificate |
| `header_check` | empty | the declared header assertions |
| `wg_handshakes_file` | absent | the file a privileged timer writes from `wg show <interface> latest-handshakes` |
| `tunnel_handshake_max_age_s` | 600 | the handshake age above which a tunnel is a fault. With a tunnel configured, the start is refused below `poll_interval_s` + `timeout_ms` rounded up to whole seconds + 180 |
| `health_last_ok_max_age_s` | three poll intervals | the age above which a node's last 200 is flagged |
| `environment` | absent | `dev`, `staging` or `prod`. A dirty build is refused anywhere but `dev`, and a pre-release is refused on `prod` |
| `expected_deploy_file` | absent | the deploy's record of what it expects on each node, resolved against the site directory |

**Table 3: the `[monitor]` keys.**

A key under `[monitor]` outside Table 3 stops the start with exit 2, because `Fleet` in `m6-monitor/src/fleet.rs` carries `deny_unknown_fields`.
`poll_interval_s` is a top-level key with a default of 30 seconds, and `[monitor] poll_interval_s` is refused the same way.
`fleet::poll_interval_s` is the one function the poller and the handshake check both read it through.

`[monitor] environment` is the deployment's own declaration, like `journal_dir`.
Absent, the monitor reads the `environment` field of the deploy's record.
With neither, the two version rules cannot be judged and the check says so once.

### 5.1 The deploy's record

The monitor checks each run against what the deploy meant to ship: the release version and the sha256 of every rendered config file.
The deploy writes that record and the monitor only reads it, so no path or value is compiled into this crate.
No writer of this file exists in this repository, so until a deployment supplies one the monitor prints `waiting for the deploy side` and every comparison against the record waits with it.

`[monitor] expected_deploy_file` names the file.
A relative path resolves against the monitor's site directory, and `--check` resolves it against the config file's directory, the same rule `journal_dir` follows.
A file that does not exist is a state and the monitor starts.
The check prints `waiting for the deploy side` and adds one warning that says so, and it adds no per-run finding for the values it could not compare.
A file that exists and cannot be read is a Fault.
A monitor with no `expected_deploy_file` key gets a warning too, saying that no run's version, config sha256 or decided facts are checked.

Table 4 gives the shape, which is JSON.

| field | type | what it says |
|---|---|---|
| `environment` | string, optional | `dev`, `staging` or `prod`, used when `[monitor] environment` is not set |
| `deploy_in_progress` | boolean, default false | whether a deploy is running now. A restart is expected only when this is true and the previous run wrote a shutdown line |
| `nodes` | object, keyed by node | one `NodeExpectation` per node the start-up record names |
| `nodes.<node>.apps` | object, keyed by app | one `AppExpectation` per process on that node |
| `apps.<app>.version` | string, required | the full SemVer the deploy shipped, build metadata included |
| `apps.<app>.config` | object, role to sha256 | the sha256 of every config file the deploy rendered |
| `apps.<app>.decided` | object, optional | the decided facts the deploy recorded, such as `socket_path`, `bind`, `process_index` or `lock_memory` |

**Table 4: the deploy's expectation file.**

The keys of `nodes` and `apps` are the ones the record itself carries: the node is the record's `node` field, or the hostname on its platform line, and the app is the record's `app` field.
A node polled as several processes may key an app as `<app>/<index>`, and that entry wins over the plain `<app>`.
Unknown fields are refused, so a deploy-side typo fails the read.

```json
{
  "environment": "prod",
  "deploy_in_progress": false,
  "nodes": {
    "origin": {
      "apps": {
        "m6-http": {
          "version": "1.16.0+abc123def456",
          "config": {"config": "<sha256>", "site": "<sha256>"},
          "decided": {"socket_path": "/run/m6/m6-http.sock"}
        }
      }
    }
  }
}
```

### 5.2 The fleet entries

Table 5 gives the keys of one fleet entry.

| key | default | what it is |
|---|---|---|
| `name` | required | the node's name, which the journal is read by and the report shows |
| `url` | required | the node's base URL, scheme and authority with an optional path prefix |
| `role` | empty | a label for the report |
| `tunnel.address` | absent | an IP address and port on the WireGuard tunnel |
| `tunnel.public_key` | absent | the peer's public key, for the handshake check. Without it the round trip is measured and the node reports a fault, because its handshake cannot be checked |
| `processes` | absent | how many `[server] process_index` processes the node runs on its shared port |

**Table 5: the keys of one `[[monitor.nodes]]` entry.**

`processes` expands one entry into `<name>/<index>` entries, each polling `tunnel.address` with its port raised by the index, so it needs that address.
A node without `processes`, or with `processes = 1`, is polled as one address.

A `[[monitor.header_check]]` entry names a `path`, a `header`, the expected value in `expect`, an optional `roles` list, and `cache_bust`, which is on by default.

`[limits] journal_line` bounds one line the reader assembles, with `start` at 64 KiB and `max` at 4 MiB by default.
`docs/m6-app-anatomy.md` section 4.2 lists the limit with every other one.

```toml
poll_interval_s = 30

[monitor]
journal_dir = "/var/log/journal/remote"
timeout_ms = 5000

[[monitor.nodes]]
name = "origin"
url  = "https://www.example.com"
role = "origin"

[[monitor.nodes]]
name = "edge-a"
url  = "https://edge-a.example.com"
role = "cache"
```

Section 6 turns to what the service answers.

## 6. The routes

The monitor serves five routes on one unix socket and has no listener.
Table 6 gives the routes and what each answers.

| route | answer |
|---|---|
| `GET /` | the page, rendered once per poll, status 503 while the digest's level is `Fault` |
| `GET /digest` | the digest as JSON, with `snapshot_age_s`, `snapshot_at`, `poll_interval_s` and `stale` added at the request |
| `GET /check` | the report as text, with the age of the reading or a `STALE:` line first |
| `GET /history` | the poll history as paged JSON Lines |
| `GET /crawlers` | the crawler sightings as paged JSON Lines |

**Table 6: the five routes.**

The socket is `/run/m6/<config stem>.sock` (`socket_path_from_config` in `m6-core/src/server.rs`), so a config file named `m6-monitor.conf` gives `/run/m6/m6-monitor.sock`.
`M6_SOCKET_OVERRIDE` replaces that path.

The monitor has no access control of its own.
An m6-http in front of the socket protects a route with its `require` rule, and `docs/m6-user-guide.md` section 7.10 gives the backend and the two routes that do it.

`/history` and `/crawlers` answer 503 with `history is off: set [monitor] history_file to keep one` when no history file is configured.
Both take `from` and `to` as RFC 3339 times, an optional `node`, and a range of at most 31 days.
A page holds at most 4 MiB and 10,000 rows by default, and a `Link` header with `rel="next"` follows while rows remain.

```bash
curl --unix-socket /run/m6/m6-monitor.sock http://localhost/digest
```

Section 7 says what the digest and the check contain.

## 7. The digest and the check

One `digest::build` call turns a finished poll into the digest that every route serves.
Table 7 gives each part of the digest and the input it came from.

| digest field | source |
|---|---|
| `name`, `role`, `url` | the fleet config |
| `status` | `/health`, through `NodeReading::status` |
| `reported_node` | the `node` field of the `/health` body |
| `rtt_ms` | the `/health` round trip, connect and TLS included |
| `version`, `binary` | the metrics document's `build`, the version with the commit as its build metadata |
| `release`, `commit`, `dirty` | the same version, parsed by the `semver` crate into the release, the build metadata's commit and the `.dirty` flag |
| `uptime_s`, `host_uptime_s` | the metrics document |
| `requests_total`, `hit_rate`, the hit and miss percentiles, `backend_errors`, `pools` | the metrics document's `metrics` and `pools` |
| `load_one`, `cpus`, `memory_used`, `disk_used`, `thermal_max_c` | the metrics document's `host` |
| `cert_not_after_unix`, `cert_fingerprint`, `cert_expires_in_seconds` | the metrics document's `tls` |
| `traffic`, `traffic_error` | the traffic document |
| `tunnel` | the TCP probe and the WireGuard handshake file |
| `history` | the history file |
| `perf_error` | the journal reader |
| `startup` | the start-up records and the deploy's expectation file (section 7.1) |

**Table 7: where each part of the digest comes from.**

Version drift compares the release part of `version`, before the `+`, because SemVer 2.0.0 gives build metadata no precedence.
Build drift compares the metadata, which is the commit the node's binary was built from.
Two nodes on one release built from different commits show build drift and no version drift, and a node whose version carries no metadata counts as build drift.

`Thresholds` carries the limits the build judges against: load per CPU 2.0, memory 0.9, disk 0.8, thermal 80 C, certificate expiry 21 days, the tunnel handshake age, and the age of the last 200.

`check::render` prints the same reading as text.
HEALTH comes first, one line per node with this poll's `/health` answer and the age of its last 200 from the history, and a node past the age limit reads `FAULT` on its line and in the verdict.
PROCESSES follows when the fleet declares `processes`, and STARTUP follows that.
STARTUP gives each node's and app's current run: the version, the run id, the start time the record states, the sha256 of every config file it read, and the number of journal lines the monitor ingested for the run, with the shutdown line when there is one.
A run that replaced an earlier one also prints `previous: <run id>, shut down <reason> at <time>`, or `previous: <run id>, no shutdown recorded`, marked `[info]` for an expected restart and `[WARN: unexpected restart]` for the rest.
One run with two config files, a clean shutdown and an expected restart before it prints this, with the sha256 values abbreviated to `<sha256>`:

```text
  origin   m6-http      ok     1.16.0-rc.1+3a639b7e5f55   started 2026-11-18T02:16:20.858965Z
           run 1794968180857-37684, config config=<sha256> site=<sha256>, 4/4 line(s)
           5 line(s) ingested
           last shutdown "signal" after 32ms
           previous: 1794968120857-37600, shut down signal at 2026-11-18T02:16:19.000000Z  [info]
```

A record that never closed reads `FAULT` on its line with the number of lines it received.
The section also says when the journal tail cut records and when shutdown lines named no run.
The rest is sections A to I: A LOGGING, B PERFORMANCE, C HOST, D SECURITY AND TRAFFIC, E CRAWLERS, F CACHE HEADERS, G CONNECTION SETUP, H TUNNELS, and I HISTORY.
Section A judges the node's own log from the traffic document's `logging`, and `LoggingHealth::is_blind` faults a log silent for more than 40 seconds.

`m6-monitor --check <config>` prints the same report and exits 1 on a fault and 0 otherwise.
A config the fleet parser refuses exits 1 with the parser's message, and a service that cannot start exits 2 with `m6-monitor: <what>: <why>` on stderr.
`m6-monitor --version` prints the tag form, `v1.16.0-rc.1`, and `m6-monitor --version-full` prints one line per m6 crate in the binary, starting with `m6-monitor`.
Both answer before a config path is required.

### 7.1 The start-up checks

The `startup` object carries each run the journal holds and the state of the deploy's record.
It carries `environment`, `expected_file`, `expected_state`, `expected_error`, `deploy_in_progress`, `cut_records`, `orphan_shutdowns`, `journal_unavailable` and `runs`.
`expected_state` is `loaded`, `missing`, `not_configured` or `unreadable`.
Each run carries `node`, `app`, `process_index`, `run_id`, `version`, `tag`, `started_at`, `config`, `config_paths`, `lines`, `ingested_lines`, `complete_lines`, `state`, `shutdown`, `previous_run_id`, `previous_shutdown` and `restart`.
Table 8 gives every check, the reference it is judged against, and its severity.

| check | reference | severity |
|---|---|---|
| the build's metadata contains `.dirty` | the environment, refused anywhere but `dev` | Fault |
| the version carries a pre-release | the environment, refused on `prod` | Fault |
| the version is not SemVer | the version itself, before any rule | Fault |
| the version differs from the deploy's recorded release, build metadata included | the deploy's record, parsed with `semver` | Fault |
| the deploy's recorded version is not SemVer | the deploy's record | Fault |
| a config file's sha256 differs | the deploy's record | Fault |
| a config role the deploy recorded has no `config_file` line | the deploy's record | Fault |
| a decided fact differs from the deploy's record, or the record carries no such fact | the deploy's record | Fault |
| the `complete` count differs from the lines received | the record itself | Fault |
| a record never wrote its `complete` line | the record itself | Fault |
| the deploy's expectation file exists and cannot be read | the file | Fault |
| a new run id replaced an older run | whether a deploy is in progress and the previous run wrote a shutdown line | Warn |
| a node's kernel, OS, CPU or memory differs from its peers | the other nodes' platform lines | Warn |
| the deploy's expectation file is not configured | the monitor's config | Warn |
| the deploy's expectation file does not exist | the file | Warn |
| the deploy's record names no expectation for this node | the deploy's record | Warn |
| the deploy's record names no expectation for this app on this node | the deploy's record | Warn |
| the record read a config role the deploy recorded no sha256 for | the deploy's record | Warn |
| no environment is set by the monitor or the deploy's record | the monitor's config and the deploy's record | Warn |
| the monitor's environment differs from the deploy's record | the two declarations | Warn |
| the record names no node and its platform line reports no hostname | the record itself | Warn |

**Table 8: the checks on the start-up record.**

A finding about the deploy's record or the environment names node `monitor`.
The other findings name the record's node, its run id and the fact, so a reader can act on one without opening the node.

A restart is expected only when the deploy's record says a deploy is in progress and the previous run wrote a shutdown line.
An unexpected restart quotes the previous run's id and, when it wrote one, the shutdown reason, its time and the uptime.
Every restart carries `previous_shutdown` in the run's `/digest` entry: `{reason, at, uptime_ms}` from the previous run's shutdown line, or `null` when it wrote none.
The entry's `restart` is `info` for an expected restart and `warn` for an unexpected one, and only the second is a finding.
`ingested_lines` is the raw count of journal lines the monitor read for the run's id: the start-up lines, any replay of them from the journal, and the shutdown line. `lines` counts the start-up lines alone.
Without the deploy's record the monitor cannot claim or deny a deploy, so it treats a restart with a shutdown line as expected.

A record that never closed is reported under the run it was, with the number of lines it received, and it stays in the report while it is one of the 32 most recent unfinished records.

Section 8 turns to the file that keeps those readings.

## 8. History

With `[monitor] history_file` set, every finished poll is kept in one SQLite file, and that file is the record that outlives a node's journal.

`history::start` opens the file, applies the migrations, and returns a `History` the loop owns.
A batch is one poll: one row in `health` per node, with `health_channel` and `health_net` rows under it, the findings in `poll_finding`, and one `crawl_window` per node window with its sightings.

When the file's used bytes pass `history_max_mb`, the oldest `poll` rows are deleted 256 at a time until the file is 5% under the cap.
Nothing expires by age, and the file is never vacuumed.

A write that meets a locked file leaves its batch for the next poll, so the loop never waits on the file.
A write that fails for another reason is reported as `failing` in the digest's `history` object, and `behind`, `prune_failed` and `over_cap` are reported there too.

Section 9 says where the crate sits.

## 9. Where it fits

m6-monitor links no m6 crate other than `m6-core` (`m6-monitor/Cargo.toml`), and runs as one process with one event loop.
The journal child is the only process it starts, and it is killed at stop.

The two documents it reads are defined in `m6_core::monitoring` and written by `m6-log`, and the start-up record is `m6_core::startup`'s, written by the same log.
[`docs/m6-http.md`](m6-http.md) section 11 gives the documents and `/health`.
[`docs/m6-log.md`](m6-log.md) gives the line format and `M6_TYPE`.
[`docs/m6-user-guide.md`](m6-user-guide.md) section 7.9 gives the deployment side: the journal upload, the directory, and the unit.

This document is the crate's reference: sections 2 and 3 are the contract with the two inputs, section 5 is the whole configuration, and section 7 is every number the digest reports together with where it came from.
