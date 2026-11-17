# m6-log: buffered JSON lines for one event loop

m6-log writes one m6 process's log.
It keeps the line format m6's consumers read, and it removes the writer thread, the lock and the heap allocation from each line.
A log call copies one JSON line into a buffer that start made, and the process's own loop writes that buffer when it is nearly full, on the accounting tick and at stop.

The library is `m6-log`.
m6-core re-exports it as `m6_core::log` and adds `watch`, which is the one piece that needs the reactor.
The library has no thread, no channel and no lock.

## The line

Every line is one JSON object and one newline, which is the JSONL form.
Table 1 lists the keys of one line.

Table 1: the keys of one line.

| key | value |
|---|---|
| `timestamp` | UTC ISO 8601 with six fractional digits, for example `2026-11-13T16:55:49.489591Z` |
| `level` | `ERROR`, `WARN`, `INFO`, `DEBUG` or `TRACE` |
| `fields` | the event's message and its fields |
| `target` | the operational line's module, the metrics line's `m6_http::stats`, absent from the access line |
| `type` | `event`, `perf`, `traffic` or `access` (see below) |

The timestamp cache holds `YYYY-MM-DDTHH:MM:SS` for one second in a 20-byte array.
Each line reads `SystemTime::now()` once and appends that second's cached text and its own six fractional digits, so the civil date is formatted once per second at most.

Strings are escaped as JSON requires.
The quote and the backslash are escaped, and every byte from 0x00 to 0x1f is written as `\u00xx`.
Non-ASCII UTF-8 passes through unchanged.
Keys and targets use the same escaper as the string fields.
A string field writes at most `[limits.log_field] max` escaped bytes inside its quotes, 8 KiB by default.
A value that needs more refuses its whole line: nothing is cut, the line counts as dropped, and one line on stderr names the key and the file.
The Sink's buffer starts at `[limits.log_buffer] start`, 16 KiB by default, doubles when a line does not fit and the destination is blocked, and keeps its size.
A line past `[limits.log_buffer] max`, 1 MiB by default, is refused the same way.
A message, a `%display` field and a `?debug` field are formatted straight into the buffer through the escaping adapter, with no `String`.

The `type` key names what kind of line it is, so a reader filters on one key rather than on a target and a message that a refactor can rename.
Its four values are below.
`level` keeps the severity: a warning is `type` `event` with `level` `WARN`.

- `event`: an operational message, at any level.
- `perf`: the periodic metrics line, which carries the document the old `/perf` endpoint answered.
- `access`: a per-request analytics line, the request row or the monitor row.
- `traffic`: the periodic traffic summary, which carries the document the old `/traffic` endpoint answered.

The type is a static word written into the buffer, so a line's type costs no allocation.

`Line::json` writes one field as a JSON value rather than as a string: a nested object or array, written by the value's `Serialize` implementation through `serde_json` straight into the buffer.
A nested value is bounded by the line's own buffer and not by the string field cap, because a document cut at 8 KiB is not JSON.
The largest line m6 writes is the metrics document, measured at 26,143 bytes with every section at its maximum on 2026-11-17, against the 1 MiB buffer maximum.
`docs/m6-http.md` says which sections dominate it.

A field with no value is omitted, as tracing omitted it.
Key order is free for a JSON reader.
m6-log writes the message after the other fields when a macro builds the line, because the macro reads its fields before the message.

## Buffering and loss on crash

Table 2 lists the four times the buffer is written.

Table 2: when the buffer is written.

| trigger | what happens |
|---|---|
| the next line would overflow it | the complete lines are written, then the line is appended |
| the accounting tick, once a second | the Sink is flushed |
| a warning or an error | the line is appended and the Sink is flushed at once |
| stop | everything is written, then the dropped-count line |

The buffer is 16 KiB, made in one `Box<[u8]>` at start.
A line that cannot fit even in an empty buffer is dropped and counted, and is never split.
A killed process loses at most the lines of the last second or the last 16 KiB, as nginx's buffered access log does.
Warnings and errors are written at once, so the reason for a crash is in the old file, in the part that was not lost.

The Sink holds complete lines at the front of its buffer and the line being built after them.
A flush during a line moves the partial line to the front and writes only the complete lines.
That is what keeps a write from splitting a line.

## Levels

`init(level)` sets the level once, at start.
The level is fixed for the life of the process, because configuration is read at start.
`error!`, `warn!`, `info!`, `debug!` and `trace!` compare against it before they build anything, and a line below the level costs one load and one compare.

The macro syntax is the common part of tracing's: an optional `target: "..."`, then `key = value`, `key = %display` and `key = ?debug` fields, then the message with its format arguments.
A value's Rust type decides its JSON type: a string is a JSON string, an integer is a JSON number, a boolean is a JSON boolean and `None` writes no field.
A call site that needs more than that writes its fields with `Line` directly.

Every line goes to the one Sink, stdout, which `m6_core::log::with_operational` reaches.
The access line is written with `Line` field by field, has no target, and its `type` is `access`.
The metrics line and the traffic line are written the same way, with `type` `perf` and `traffic` and the targets `m6_http::stats` and `m6_http::traffic`.

## The journal

Under systemd, m6-log writes each line to journald's own socket instead of to stdout.
The journal is still the destination, and the journal rotates, so there is no file to rename and no signal to send.
A service that needs a file sets `StandardOutput=append:` in its unit and lets the journal own the rotation.

Table 3 lists what one datagram carries.
The wire format is the one `sd_journal_send` writes: every field ends with a newline, the last included, and a field whose value contains a newline uses the binary form.
The binary form is the field name, a newline, an eight-byte little-endian length, the value and a newline.
journald discards an unterminated last field and does not decode a backslash escape, so both parts are required for the journal to store the line.
m6 writes that format directly and links no journal library.

Table 3: the fields of one journal datagram.

| field | value |
|---|---|
| `PRIORITY` | the level as a syslog priority: `3` error, `4` warning, `6` info, else `7` |
| `M6_TYPE` | the line's `type`: `event`, `perf`, `access` or `traffic` |
| `SYSLOG_IDENTIFIER` | the program's own name, from `argv[0]` |
| `MESSAGE` | the whole JSON line, `type` included, as stdout would carry it |

`journalctl M6_TYPE=perf` therefore reads the metrics lines without parsing JSON, and `journalctl -o json` carries the same fields to a remote journal.

Detection is one platform fact, `m6_platform::journal_stream()`: systemd sets `JOURNAL_STREAM` on a unit whose stdout it wired to the journal, and nothing else sets it, so a shell, a launchd plist and a test harness all take the stdout path.
A socket that cannot be opened at start prints one line on stderr and the process writes to stdout.
A send that fails leaves that line to stdout, which under systemd is the same journal, and the process writes to stdout from then on: no line is dropped and none is sent twice.

A datagram is capped at 64 KiB, the size a unix datagram carries on every supported kernel.
A line that would need more is refused the way a line over the buffer maximum is refused: it counts as dropped and one line on stderr names the limit.
`sd_journal_send` sends such a message through a `memfd` instead. m6 refuses it, because a refused line is visible in the dropped count and a second transport is not.

## Dropped-line counting

A write to stdout can return `EAGAIN` when the destination is full.
The bytes stay in the buffer and the loop waits for writability, which `watch` in m6-core registers under the service's `LOG` token.
While the destination is full, the buffer is not retried before a flush.
New lines are appended while the buffer has room, and the line that does not fit in the full buffer is dropped and counted.
When the destination drains, the next flush writes the buffered lines and then one line at `WARN` with `message` `"log lines dropped"` and the `dropped` count.
If that report line itself cannot be written, the count stays pending and the next flush reports the whole loss again.
`Sink::dropped()` reports the total for the life of the Sink.

A write error other than `EAGAIN` counts the complete lines it could not send as dropped.
A terminal and a regular file never return `EAGAIN`, so neither is ever registered with a loop.

## The loop's calls

A service's loop makes five calls: three while it runs and two at its edges.
Table 4 lists them.

Table 4: the calls a service makes.

| call | where |
|---|---|
| `init(level)` | at start, before the loop |
| `tick(now)` | on the one-second accounting tick, with the time the loop already read |
| `watch(poll.registry(), LOG)`, its error logged | after one turn's events, in `m6_core::log` |
| `flush()` | in the match on the token, for `LOG` |
| `stop()?` | after the loop, where blocking is allowed |

`watch` lives in `m6-core`, over `Sink::fd` and `Sink::wants_write`.
It registers stdout for writability only while a write has returned `EAGAIN`, and removes it when the Sink has drained.
A line waiting for the tick is buffered, not watched, so a regular file and a terminal are never registered.
A registration failure is logged and the loop serves on.

## What it copies

Table 5 lists the behaviour m6-log borrows and the reason for each.

Table 5: the behaviour m6-log copies from nginx and HAProxy.

| behaviour | taken from | reason |
|---|---|---|
| a buffered log written when it would overflow and on an interval | nginx `access_log ... buffer=16k flush=1s` | one write for many lines, and a bounded loss on a crash |
| the error log written at once | nginx `error_log` | the reason for a failure survives the failure |
| JSON escaping of every string | nginx `log_format ... escape=json` | one escaper and no invalid line |
| a cached date string, not formatted per line | nginx `ngx_time_update` | one civil conversion per second |
| never wait on a full destination, keep and count what cannot be written | HAProxy's non-blocking log sinks | a log call cannot stall the loop |

m6-log leaves out gzip of log files, conditional logging, syslog, a log format language and any destination other than stdout, because the journal is the destination.

## Where it fits

m6-log is the writing half of logging.
m6-core re-exports it as `m6_core::log` and adds `watch`, the one piece that needs the reactor.
Each service's own loop supplies the tick, the token and the stop, in plain sight, as every other part of its loop does.
