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
| `target` | the operational line's module, absent from the analytics line |

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

A field with no value is omitted, as tracing omitted it.
Key order is free for a JSON reader.
m6-log writes the message after the other fields when a macro builds the line, because the macro reads its fields before the message.

## Buffering and loss on crash

Table 2 lists the four times the buffer is written.

Table 2: when the buffer is written.

| trigger | what happens |
|---|---|
| the next line would overflow it | the complete lines are written, then the line is appended |
| the accounting tick, once a second | both Sinks are flushed |
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

`init(level, analytics)` sets the level once, at start.
The level is fixed for the life of the process, because configuration is read at start.
`error!`, `warn!`, `info!`, `debug!` and `trace!` compare against it before they build anything, and a line below the level costs one load and one compare.

The macro syntax is the common part of tracing's: an optional `target: "..."`, then `key = value`, `key = %display` and `key = ?debug` fields, then the message with its format arguments.
A value's Rust type decides its JSON type: a string is a JSON string, an integer is a JSON number, a boolean is a JSON boolean and `None` writes no field.
A call site that needs more than that writes its fields with `Line` directly.

The analytics line is written with `Line` field by field into the analytics Sink, which `with_analytics` reaches.
That line has no target.

## Reopen

SIGUSR1 reaches `reopen()`, which flushes the analytics Sink and opens its path again.
The flush comes first, so every line before the signal is in the file the rotation tool renamed, and no line reaches the old file after the new one is open.
A process with no analytics file does nothing on SIGUSR1 and answers `Ok`.

## Dropped-line counting

A write to stdout can return `EAGAIN` when the destination is full.
The bytes stay in the buffer and the loop waits for writability, which `watch` in m6-core registers under the service's `LOG` token.
While the destination is full, the buffer is not retried before a flush.
New lines are appended while the buffer has room, and the line that does not fit in the full buffer is dropped and counted.
When the destination drains, the next flush writes the buffered lines and then one line at `WARN` with `message` `"log lines dropped"` and the `dropped` count.
If that report line itself cannot be written, the count stays pending and the next flush reports the whole loss again.
`Sink::dropped()` reports the total for the life of the Sink.

A write error other than `EAGAIN` counts the complete lines it could not send as dropped.
A regular file never returns `EAGAIN`, so the analytics Sink is never registered with a loop.

## The loop's calls

A service's loop makes six calls: three while it runs, one on a signal and two at its edges.
Table 3 lists them.

Table 3: the calls a service makes.

| call | where |
|---|---|
| `init(level, analytics)` | at start, before the loop |
| `tick(now)` | on the one-second accounting tick, with the time the loop already read |
| `watch(poll.registry(), LOG)`, its error logged | after one turn's events, in `m6_core::log` |
| `flush()` | in the match on the token, for `LOG` |
| `reopen()`, its error logged | when `Signal::ReopenLogs` arrives |
| `stop()?` | after the loop, where blocking is allowed |

`watch` lives in `m6-core`, over `Sink::fd` and `Sink::wants_write`.
It registers stdout for writability only while a write has returned `EAGAIN`, and removes it when the Sink has drained.
A line waiting for the tick is buffered, not watched, so a regular file and a terminal are never registered.
A registration failure is logged and the loop serves on.

## What it copies

Table 4 lists the behaviour m6-log borrows and the reason for each.

Table 4: the behaviour m6-log copies from nginx and HAProxy.

| behaviour | taken from | reason |
|---|---|---|
| a buffered log written when it would overflow and on an interval | nginx `access_log ... buffer=16k flush=1s` | one write for many lines, and a bounded loss on a crash |
| the error log written at once | nginx `error_log` | the reason for a failure survives the failure |
| JSON escaping of every string | nginx `log_format ... escape=json` | one escaper and no invalid line |
| a cached date string, not formatted per line | nginx `ngx_time_update` | one civil conversion per second |
| never wait on a full destination, keep and count what cannot be written | HAProxy's non-blocking log sinks | a log call cannot stall the loop |
| reopen the file on SIGUSR1 | nginx `USR1` | rotation without a restart |

m6-log leaves out gzip of log files, conditional logging, syslog, a log format language and more than two destinations per process, because m6 has no use for them.

## Where it fits

m6-log is the writing half of logging.
m6-core re-exports it as `m6_core::log` and adds `watch`, the one piece that needs the reactor.
Each service's own loop supplies the tick, the token and the stop, in plain sight, as every other part of its loop does.
