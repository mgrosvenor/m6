# m6-log: one stream, one type on every line

m6-log writes one process's log as JSON lines.
Every line leaves through one sink.
The sink writes stdout, and under systemd it writes journald's native socket as one datagram per line.
A top-level `type` names what a line carries.
A reader selects the metrics, the traffic summary, the request rows or the operational events on that one key, without parsing a message.
The per-line path has no thread, no channel and no lock, and `m6-log/tests/no_allocation.rs` carries 700,000 lines at zero allocations.

This page describes the line and its six types (section 1), the size budget (section 2), the buffering and the loss on a crash (section 3), and the levels and the macros (section 4).
It then describes the journal datagram (section 5), dropped-line counting (section 6), the calls a service's loop makes (section 7), where the crate fits (section 8), and the start-up and shutdown recovery record (section 9).

## Contents

1. [The line](#1-the-line)
2. [Size limits](#2-size-limits)
3. [Buffering and loss on crash](#3-buffering-and-loss-on-crash)
4. [Levels and macros](#4-levels-and-macros)
5. [The journal](#5-the-journal)
6. [Dropped-line counting](#6-dropped-line-counting)
7. [The loop's calls](#7-the-loops-calls)
8. [Where it fits](#8-where-it-fits)
9. [The recovery record](#9-the-recovery-record)

## 1. The line

Every line is one JSON object and one newline on stdout.
The object carries four fixed keys and one optional key, and a JSON reader may take them in any order.
`Type` in `m6-log/src/line.rs` decides the value of the `type` key.
Table 1 gives the six values of that key and what each line carries.
Take from it that the six kinds share one stream and one line shape.

| `type` | what the line carries | written by |
|---|---|---|
| `event` | an operational message at any level | any service, through a macro or `Line` |
| `perf` | the metrics document, nested under `fields.perf`, that the removed `/perf` endpoint answered | m6-http at `[stats].interval_s`, ten seconds by default, through `log_perf` in `m6-http/src/stats.rs` |
| `traffic` | the traffic summary, nested under `fields.traffic`, that the removed `/traffic` endpoint answered | m6-http at `[stats].traffic_interval_s`, one minute by default, through `log_traffic` in `m6-http/src/stats.rs` |
| `access` | one per-request row, with `message` `request` or `monitor` | m6-http's analytics writer, through `write_request_line`, `write_monitor_line` and `write_rate_limited_line` in `m6-http/src/analytics.rs` |
| `startup` | one part of the start-up record: identity, invocation, a config file, a config section, the platform facts, or the completeness count | `m6-core/src/startup.rs`, once at start |
| `shutdown` | the clean exit: the same `run_id`, the uptime and the reason | `m6-core/src/startup.rs`, from `Lifecycle::complete` or `startup::shutdown` |

**Table 1: the six values of `type`, the key that names what a line carries.**

`StatsConfig` in `m6-http/src/config.rs` reads `[stats].interval_s` and `[stats].traffic_interval_s`.
`level` keeps the severity, so a warning is `type` `event` with `level` `WARN`.
A reader selects on `type` in the JSON, or on `M6_TYPE` in the journal datagram (section 5).
The two words are the same value.

Table 2 gives the top-level keys in the order `Line::end` writes them.

| key | value |
|---|---|
| `timestamp` | UTC ISO 8601 with six fractional digits, for example `2026-11-13T16:55:49.489591Z` |
| `level` | `ERROR`, `WARN`, `INFO`, `DEBUG` or `TRACE` |
| `fields` | the message and the line's fields, as one JSON object |
| `target` | the module, crate or subsystem that wrote the line, written only by a line that names one |
| `type` | one of Table 1's six words, written last |

**Table 2: the top-level keys of one line, in written order.**

Take from Table 2 that every line has a timestamp, a level, fields and a type, and that `target` alone is optional.
An access line names no target.
The metrics line's target is `m6_http::stats`, and the traffic line's is `m6_http::traffic`.

This line was captured from a running stack and is the golden test's `operational_warn` line (`m6-log/tests/golden/operational_warn.jsonl`).

```json
{"timestamp":"2026-11-13T16:55:49.489428Z","level":"WARN","fields":{"message":"unmatched path","path":"/missing"},"target":"m6_core::site","type":"event"}
```

`event` is the default type, so the macro that wrote that line named no type.

Strings are escaped as JSON requires: the quote and the backslash are escaped, every byte from 0x00 to 0x1f is written as `\u00xx`, and non-ASCII UTF-8 passes through unchanged.
Keys and targets use the same escaper as a string field.
The timestamp cache holds `YYYY-MM-DDTHH:MM:SS` for one second in a 20-byte array.
Each line reads the clock once and appends that second's cached text and its own six fractional digits, so the civil date is formatted once per second at most.

A reader who has the keys can read any line.
Section 2 gives the sizes that decide whether a line is written.

## 2. Size limits

Three caps bound a line: its longest string field, its sink buffer and its journal datagram.
Each refuses the whole line at its top.
Table 3 gives the numbers a log budget needs, with the defaults and the recorded worst case.
Take from it that the tightest cap is the 8 KiB string field by default, and that the 64 KiB datagram is the ceiling under systemd.

| quantity | bytes | source |
|---|---|---|
| one string field, escaped bytes inside its quotes | 8,192 (8 KiB) | `[limits.log_field] max`, and a longer field rolls the whole line back |
| sink buffer at start | 16,384 (16 KiB) | `[limits.log_buffer] start`, and the buffer doubles when a line does not fit |
| sink buffer maximum | 1,048,576 (1 MiB) | `[limits.log_buffer] max`, and a longer line is refused whole |
| metrics document with every section at its maximum | 26,143 | the measured worst case, nested under `fields.perf` |
| one journal datagram | 65,536 (64 KiB) | `DATAGRAM_MAX`, and a larger datagram refuses the line |

**Table 3: the numbers that size a log budget, with the source of each.**

A string field is capped at `[limits.log_field] max`, which is 8 KiB by default.
The cap counts escaped bytes inside the quotes.
`LogLimits` in `m6-log/src/sink.rs` holds the field cap, the buffer start and the buffer maximum.
The service start in `m6-core/src/site.rs` reads `[limits.log_buffer]` and `[limits.log_field]` from `m6-core/src/limits.rs` and passes them to `set_limits` in `m6-log/src/sink.rs`.
m6-http's start in `m6-http/src/main.rs` does the same.
A value that needs more rolls the whole line back: the line counts as dropped, and m6-log writes one line on stderr naming the limit and the config file.
A message, a `%display` field and a `?debug` field go through the same escaping adapter and the same cap, with no `String`.

A large document goes in `fields` as a nested JSON value.
`Line::json` in `m6-log/src/line.rs` writes the value through its `Serialize` implementation straight into the buffer.
The nested value is bounded by the line buffer, `[limits.log_buffer] max`, so the metrics document fits the 1 MiB maximum.
A nested value the buffer cannot hold rolls the whole line back.

With the defaults, the first metrics line grows the sink from 16 KiB to 32 KiB, and the sink keeps that size.
A line past `[limits.log_buffer] max` is refused whole and counted as dropped.

Under systemd the datagram is the tighter ceiling.
`DATAGRAM_MAX` in `m6-log/src/journal.rs` is 64 KiB.
A datagram is 65,536 bytes in all, and it carries the JSON line plus four field names and the program's name.
A line that makes the datagram larger is refused as a datagram even when the 1 MiB buffer would hold it.
The metrics document measured 26,143 bytes on 2026-11-17 with every section at its maximum.
The `host` block is a snapshot of the machine, so another host measures a different total.

Those are the numbers a budget needs.
Section 3 describes when the buffer is written and what a crash can lose.

## 3. Buffering and loss on crash

Table 4 lists the four times a sink writes its buffer.
Take from it that a line can wait one second, and that a warning or an error does not wait.

| trigger | what happens |
|---|---|
| the next line would overflow the buffer | the complete lines are written, then the line is appended |
| the accounting tick, once a second | `tick(now)` flushes the sink |
| a warning or an error | the line is appended and the sink is flushed at once |
| stop | everything is written, then the dropped-count line |

**Table 4: when a sink writes its buffer.**

`Sink::stdout` in `m6-log/src/sink.rs` makes the buffer as one `Vec<u8>` at `[limits.log_buffer] start`, 16 KiB by default.
A line that does not fit even in an empty buffer is dropped and counted, and the sink never splits it.
A killed process loses the lines written since the last tick, at most one buffer's worth.
A warning or an error flushes the sink as it is written, so it does not wait for the tick.

The sink holds complete lines at the front of its buffer and the line being built after them.
A flush during a line moves the partial line to the front and writes only the complete lines, so a write never splits a line.

The buffer bounds what a crash can lose.
Section 4 describes the level that decides whether a line is built at all.

## 4. Levels and macros

`init(level)` in `m6-log/src/lib.rs` sets the level once, at start.
A change takes a restart, because the configuration is read at start.
`error!`, `warn!`, `info!`, `debug!` and `trace!` in `m6-log/src/macros.rs` check the level before they build the line, so a line below the configured level costs no allocation.

The macro syntax is an optional `target: "..."`, then `key = value`, `key = %display` and `key = ?debug` fields, then the message with its format arguments.
A value's Rust type decides its JSON type: a string is a JSON string, an integer is a JSON number, a boolean is a JSON boolean, and `None` writes no field.
A call site that needs more writes its fields with `Line` directly.

Every line goes to the one sink.
`init` opens that sink on stdout, and section 5 describes the journal path it takes under systemd.

## 5. The journal

Under systemd every line leaves as one datagram on journald's native socket.
Everywhere else it leaves on stdout.
The analytics file and the `SIGUSR1` reopen that older releases used are gone.
`Signal` in `m6-platform/src/lib.rs` carries `Shutdown` only, and `init` opens one sink.
`[analytics] enabled` in `m6-http/src/config.rs` still turns the access rows on and off.
Those rows now go to the one stream as `type` `access`.

Table 5 lists the four fields of one datagram.
Take from it that `M6_TYPE` and `PRIORITY` are readable without parsing the JSON, and that `MESSAGE` still carries the whole line.

| field | value |
|---|---|
| `PRIORITY` | the level as a syslog priority: `3` error, `4` warning, `6` info, `7` debug and trace |
| `M6_TYPE` | the line's `type`: `event`, `perf`, `access`, `traffic`, `startup` or `shutdown` |
| `SYSLOG_IDENTIFIER` | the program's own name, from the file name in `argv[0]`, or `m6` when that is empty |
| `MESSAGE` | the whole JSON line, `type` included, without the newline that separates lines on stdout |

**Table 5: the fields of one journal datagram.**

The wire format is the one `sd_journal_send` writes: every field ends with a newline, the last included, and a field whose value contains a newline uses the binary form.
The binary form is the field name, a newline, an eight-byte (64-bit) little-endian length, the value and a newline.
journald discards an unterminated last field, and it decodes neither a literal newline in the text form nor a backslash escape, so a value with a newline needs the binary form.
`Journal` in `m6-log/src/journal.rs` writes that format directly and links no journal library.

`journalctl M6_TYPE=perf -o json` selects the metrics datagrams on the native field and prints them as JSON.
This command is illustrative: run it on a host whose journal received an m6 service.

```sh
journalctl M6_TYPE=perf -o json
```

`m6-monitor`'s journal reader in `m6-monitor/src/journal.rs` starts `journalctl` with `--lines=200`, `--output=json` and `--directory <dir>`, and adds `--follow` in the running monitor.
It filters on `M6_TYPE=perf`, `M6_TYPE=traffic`, `M6_TYPE=startup` and `M6_TYPE=shutdown`, so one child carries the four types.
The native fields are the monitor's input.

Detection is one platform fact.
`journal_stream()` in `m6-platform/src/lib.rs` reads `JOURNAL_STREAM`.
The variable is set by systemd on a unit whose stdout it wired to the journal, and a shell, a launchd plist and a test harness leave it unset.
When it is set, `Journal::open` connects to `$XDG_RUNTIME_DIR/systemd/journal/socket` if that socket exists, and to `/run/systemd/journal/socket` otherwise.
The choice is made once, in `init`.

A socket that is not there at start has two cases.
Outside systemd, m6-log opens no socket and writes to stdout.
Under systemd, a socket that cannot be opened prints one line on stderr naming the path.
The process writes to stdout.
A socket that cannot be made non-blocking does the same.

A send that fails leaves that line to stdout, which under systemd is the same journal.
The sink stays on stdout for the rest of the process, and every line is sent once.
Those later lines reach the journal as plain stdout, so a `M6_TYPE` filter does not match them.

A datagram is capped at 64 KiB, the size a unix datagram carries on every supported kernel.
A line that would need more is refused the way a line over the buffer maximum is refused.
It counts as dropped, and m6-log writes one line on stderr naming the limit.
`sd_journal_send` sends such a message through a `memfd`.
m6 refuses it, because a refused line is visible in the dropped count.

The journal path is one datagram per line.
Section 6 describes what a full destination costs.

## 6. Dropped-line counting

A line counts as dropped when it is refused for its field, its buffer or its datagram.
On stdout, a write can also return `EAGAIN` when the destination is full.
The bytes stay in the buffer, and `watch` in `m6-core/src/log.rs` registers the descriptor under the service's `LOG` token.
While the destination is full, the sink does not retry before a flush.
New lines are appended while the buffer has room, and the line that does not fit is dropped and counted.
When the destination drains, the next flush writes the buffered lines and then one line at `WARN`.
That line has `message` `log lines dropped`, the `dropped` count, `target` `m6_log` and `type` `event`.
If the report line itself cannot be written, the count stays pending and the next flush reports the whole loss again.
`Sink::dropped()` in `m6-log/src/sink.rs` reports the total for the life of the sink.

A write error other than `EAGAIN` counts the complete lines it could not send as dropped.
A terminal and a regular file never return `EAGAIN`, so neither is ever registered with a loop.

The count survives for the life of the sink.
Section 7 lists the calls that keep it current.

## 7. The loop's calls

A service's loop makes five calls: three while it runs and two at its edges.
Table 6 lists them.
Take from it that logging needs one arm in the loop's match and one call after its events.

| call | where |
|---|---|
| `init(level)` | at start, before the loop |
| `tick(now)` | on the one-second accounting tick, with the time the loop already read |
| `watch(poll.registry(), LOG)` | after one turn's events, with its error logged |
| `flush()` | in the match on the `LOG` token |
| `stop()?` | after the loop, where blocking is allowed |

**Table 6: the calls a service makes to m6-log.**

`watch` lives in `m6-core/src/log.rs`, over the sink's descriptor and `wants_write`.
It registers stdout for writability only while a write has returned `EAGAIN`, and removes it when the sink has drained.
A line waiting for the tick is buffered and not watched, so a regular file and a terminal are never registered.
A registration failure is logged and the loop serves on.

Those five calls are the whole logging surface a loop carries.
Section 8 places the crate in the workspace.

## 8. Where it fits

m6-log is the writing half of logging.
m6-core re-exports it as `m6_core::log` and adds `watch`, the one piece that needs the reactor.
Each service's own loop supplies the tick, the token and the stop, in plain sight, as every other part of its loop does.
`init` opens one sink on stdout, and under systemd that sink writes journald's native socket.
`Sink::file` in `m6-log/src/sink.rs` remains for a test or a bench that writes a line to a file, and the process logger never calls it.

One stream carries one line shape, a type on every line, and the size budget of Table 3.

## 9. The recovery record

**The start-up record is several `type=startup` lines, and a clean exit adds one `type=shutdown` line.**
A reader can tell from the log what ran, with what settings, and how it ended.
The record plus the secrets file reproduces the run, because every other setting is in the record.
This section gives the line shapes, the run id, the completeness count and the redaction rule.

The record is written once, after logging is open and after every start refusal has passed.
A service built on `Site` gets it with no call of its own.
An application with its own config calls `m6_core::startup::Record::emit` once after `log::init`, and `m6_core::startup::shutdown` once on the clean-exit path.
Table 7 gives the `part` value on each line.

| `part` | lines | what the line carries |
|---|---|---|
| `identity` | one | `app`, `version` as the full SemVer string, `tag` as the tag form, and `crates`: one `"<crate> - <semver>"` string per m6 crate in the binary, the binary first |
| `invocation` | one | `argv`, `flags` and `environment`. Every declared flag and variable is listed, and an unset one has a null `value` |
| `config_file` | one per file | `role` (`config`, `secrets` or `site`), `path`, and `sha256` of the bytes as read |
| `config_section` | one per table | `section`, the table's dotted path, `file` when the record knows it, and `settings`: every key with its `value` and `source` |
| `platform` | one | `host`, the machine's facts from `m6_core::platform::host_facts()`, `limits`, every named limit's `start`, `max` and `source`, and `decided`, the facts the process chose such as the socket it bound |
| `complete` | one | `lines`, the number of `startup` lines in the record including this one |

**Table 7: the `part` values of the start-up record.**

Every line repeats the keys in Table 8.
A line is self-contained, so a reader that filters one `part` still knows which application and run it belongs to.

| key | value |
|---|---|
| `run_id` | the process start in milliseconds since the Unix epoch, a hyphen, and the pid, for example `1763452800123-4817` |
| `seq` | the line's position in the record, from 1 |
| `part` | one of Table 7's words |
| `app` | the application's name, the same string as the `-` line of `--version-full` |
| `version` | the full SemVer string, build metadata included |
| `message` | a fixed phrase such as `startup record: identity` |
| `type` | `startup`, written last |

**Table 8: the keys every start-up line carries.**

The run id is set once at start.
A reader groups the lines of one run with one filter:

```sh
journalctl M6_TYPE=startup -o json | jq -r 'select(.run_id == "1763452800123-4817")'
```

The start time is used rather than a boot time so the record needs no host read.
A pid cannot name two processes that started in the same millisecond, so the pair is unique per run.

The completeness line counts itself.
A reader that received every line has a count equal to the number of `startup` lines it holds.
A missing line leaves a gap in `seq` and a count larger than the number received.

A config section is one table.
A `[[route]]` array is one `config_section` line per element, with paths `route[0]`, `route[1]`, so a large route table is many bounded lines rather than one long one.
An array of scalars is a value in its parent section.

The platform line keeps `host` and `decided` apart because they answer different questions.
`host` is what the machine is, so a monitor compares it across nodes: a kernel, an OS version or a total memory that differs from its peers is a finding.
`decided` is what this process chose to do with the machine, so a monitor compares it against the deploy's own record: a socket path, a bind, a process index or a memory lock that differs from what was deployed is a finding.
One line carries both so a reader of one record sees the machine and the decision together.

A setting's `source` is one of `file`, `default`, `env` or `flag`, in that order of precedence from weakest to strongest.
Every key is present, including the keys the file did not set and the application filled in.
A default is never omitted because it equals the value in use, so a later release that changes a default changes the record, which is the point.

Redaction is by type, and there is no list of names.
A config field declared `m6_core::Secret<T>` serialises as `<redacted>`, so no writer can pass it to a line.
A value whose key came from the file named by `secrets_file` is also `<redacted>`, because the file's role is the type.
A path that names a secret file is a plain setting and is logged, and the bytes of the secret never are.

A declared flag marked secret is `<redacted>` in the invocation line, and its `argv` entry is redacted in place, so the reader still sees that the flag was passed.
A command line can therefore never carry a password into the record.

A clean exit writes one `shutdown` line with the same `run_id`, `uptime_ms` and `reason`.
`Lifecycle::complete` writes it with reason `signal`.
A forced second signal writes it with reason `forced`.
A process that never wrote a start-up record writes no shutdown line, because there is no run for it to belong to.

Section 9.1 gives a measured record and the size question.

### 9.1 A measured record

Table 9 gives the largest real config measured for this release and the record it produced.
The record is many lines for exactly this reason: the largest line stays well under `[limits] journal_line`, 64 KiB, and under the 8 KiB default `[limits.log_field]`.

| tree | service and config files | lines | bytes | longest line |
|---|---|---|---|---|
| production | m6-file, its service config and the site's `site.toml` | 71 | 42,639 | 4,273 |
| staging | m6-file, its service config and the site's `site.toml` | 71 | 42,865 | 4,273 |
| production | m6-monitor, its service config and the site's `site.toml` | 64 | 34,738 | 4,276 |
| staging | m6-monitor, its service config and the site's `site.toml` | 64 | 34,920 | 4,276 |

**Table 9: the record measured from the largest rendered configs, with the line count and the longest line in bytes.**

The measurement is taken from the rendered tree through the same writer a process uses, so these are bytes the record produces rather than an estimate.
The largest is 71 lines and 42,865 bytes, and its longest line is 4,273 bytes, which is the platform line.
Every config section is under 900 bytes.
The platform line is the longest because a host reports its addresses, kernel and limits, so it is where a record grows first.
The 64 KiB `[limits] journal_line` holds it with more than 60 KiB to spare.
The line count is the reason the record is many lines: one line per table keeps the largest line at a few percent of the limit however many routes a site declares.
