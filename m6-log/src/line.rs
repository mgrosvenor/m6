//! One JSON line, built straight into a Sink's buffer.
//!
//! [`Line::begin`] opens the line with its timestamp, level and message. Each
//! field call appends one `"key":value` pair. [`Line::end`] closes the object
//! and commits the line to the Sink. Every method appends into the Sink's
//! buffer, so a line costs no allocation.

use std::cell::RefCell;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::sink::Sink;

/// What kind of line this is, written to the top-level `type` key.
///
/// Six kinds, because a reader filters on one key rather than on a target
/// and a message that a refactor can rename. `level` keeps the severity, so a
/// reader asks for `type` `event` with `level` `WARN` rather than for a `warn`
/// kind that would duplicate it.
///
/// A kind is added only with a line that is genuinely another kind.
///
/// `Startup` and `Shutdown` are the recovery record: several self-contained
/// `startup` lines sharing one `run_id`, closed by a `complete` line that says
/// how many the record has, then one `shutdown` line on a clean exit. The
/// reader filters `type=startup` (or `M6_TYPE=startup` in the journal) and
/// groups by `run_id`. See `docs/m6-log.md` Table 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    /// An operational message, at any level.
    Event,
    /// The periodic metrics line, which carries the document the old `/perf`
    /// endpoint answered.
    Perf,
    /// A per-request analytics line: the request row or the monitor row.
    Access,
    /// The periodic traffic summary, which carries the document the old
    /// `/traffic` endpoint answered.
    Traffic,
    /// One line of the start-up record: identity, invocation, a config file, a
    /// config section, the decided platform facts, or the completeness line.
    Startup,
    /// The one line a clean exit writes: the same `run_id`, the uptime and the
    /// reason.
    Shutdown,
}

impl Type {
    /// Every type, for a reader that maps the written word back through
    /// [`Type::text`].
    pub const ALL: [Type; 6] = [
        Type::Event,
        Type::Perf,
        Type::Access,
        Type::Traffic,
        Type::Startup,
        Type::Shutdown,
    ];

    /// The word written to the line's `type` key.
    pub fn text(self) -> &'static str {
        match self {
            Type::Event => "event",
            Type::Perf => "perf",
            Type::Access => "access",
            Type::Traffic => "traffic",
            Type::Startup => "startup",
            Type::Shutdown => "shutdown",
        }
    }
}

/// The severity of a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    /// A failure that stops something the operator asked for.
    Error,
    /// Something unusual the operator should read.
    Warn,
    /// The ordinary record of what the process did.
    Info,
    /// Detail a developer turns on.
    Debug,
    /// Everything, including the framework's trace.
    Trace,
}

impl Level {
    /// Every level, most severe first, for a reader that maps the written
    /// word back through [`Level::text`].
    pub const ALL: [Level; 5] = [
        Level::Error,
        Level::Warn,
        Level::Info,
        Level::Debug,
        Level::Trace,
    ];

    /// The word written to the line's `level` key.
    pub fn text(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }

    pub(crate) fn enabled(self, configured: Level) -> bool {
        (self as u8) <= (configured as u8)
    }
}

// ── The timestamp cache ─────────────────────────────────────────────────────

/// The date-and-seconds text of one second, and the second it holds.
struct Clock {
    second: u64,
    text: [u8; 20],
}

impl Clock {
    const fn new() -> Clock {
        Clock {
            second: u64::MAX,
            text: [0; 20],
        }
    }

    /// The microseconds of `now`, refreshing the cached text when the second
    /// changed.
    fn refresh(&mut self, now: SystemTime) -> u32 {
        let since = now.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
        let second = since.as_secs();
        if second != self.second {
            self.second = second;
            m6_platform::write_rfc3339_utc(second, &mut self.text);
        }
        since.subsec_micros()
    }
}

thread_local! {
    static CLOCK: RefCell<Clock> = const { RefCell::new(Clock::new()) };
}

/// Append `"YYYY-MM-DDTHH:MM:SS.ffffffZ"`'s contents for `now`.
fn push_timestamp(sink: &mut Sink, now: SystemTime) -> bool {
    let (text, micros) = CLOCK.with(|clock| {
        let mut clock = clock.borrow_mut();
        let micros = clock.refresh(now);
        (clock.text, micros)
    });
    if !sink.push(&text[..19]) {
        return false;
    }
    let fraction = [
        b'.',
        b'0' + (micros / 100_000) as u8,
        b'0' + (micros / 10_000 % 10) as u8,
        b'0' + (micros / 1_000 % 10) as u8,
        b'0' + (micros / 100 % 10) as u8,
        b'0' + (micros / 10 % 10) as u8,
        b'0' + (micros % 10) as u8,
        b'Z',
    ];
    sink.push(&fraction)
}

// ── The line ────────────────────────────────────────────────────────────────

/// Builds one JSON line straight into a [`Sink`]'s buffer.
pub struct Line<'a> {
    sink: &'a mut Sink,
    first: bool,
    target: Option<&'a str>,
    kind: Type,
    failed: bool,
}

impl<'a> Line<'a> {
    /// Open a line with its timestamp, level and message.
    pub fn begin(sink: &'a mut Sink, level: Level, message: &str) -> Line<'a> {
        Line::open(sink, level, m6_platform::wall()).message_str(message)
    }

    /// Open a line with its timestamp and level, for a caller that writes the
    /// message last. The macros use this, because their syntax puts fields
    /// before the message. JSON does not care about key order.
    #[doc(hidden)]
    pub fn empty(sink: &'a mut Sink, level: Level) -> Line<'a> {
        Line::open(sink, level, m6_platform::wall())
    }

    fn open(sink: &'a mut Sink, level: Level, now: SystemTime) -> Line<'a> {
        let mut line = Line {
            sink,
            first: true,
            target: None,
            kind: Type::Event,
            failed: false,
        };
        line = line.raw(b"{\"timestamp\":\"");
        if !line.failed && !push_timestamp(line.sink, now) {
            line.failed = true;
        }
        line.raw(b"\",\"level\":\"")
            .raw(level.text().as_bytes())
            .raw(b"\",\"fields\":{")
    }

    /// Write the line's `target`, as the operational log carries. It is written
    /// after the fields, which JSON readers cannot tell apart from before.
    pub fn target(mut self, target: &'a str) -> Self {
        self.target = Some(target);
        self
    }

    /// Write the line's top-level `type`. [`Type::Event`] is the default, so
    /// only a line that is a different kind calls this.
    ///
    /// The word is one of six static strings, written straight into the
    /// buffer, so a line's type costs no allocation.
    pub fn kind(mut self, kind: Type) -> Self {
        self.kind = kind;
        self
    }

    /// Add a string field.
    pub fn str(self, key: &str, value: &str) -> Self {
        self.field_head(key).quoted(value)
    }

    /// Add a string field only when it has a value. An absent value is left out.
    pub fn opt_str(self, key: &str, value: Option<&str>) -> Self {
        match value {
            Some(value) => self.str(key, value),
            None => self,
        }
    }

    /// Add an unsigned integer field.
    pub fn u64(self, key: &str, value: u64) -> Self {
        self.number(key, value)
    }

    /// Add a signed integer field.
    pub fn i64(self, key: &str, value: i64) -> Self {
        self.number(key, value)
    }

    /// Add a boolean field.
    pub fn bool(self, key: &str, value: bool) -> Self {
        let word = if value { &b"true"[..] } else { &b"false"[..] };
        self.field_head(key).raw(word)
    }

    /// Add a field whose value is one JSON value, written by `serde_json`.
    ///
    /// The value is written as JSON, not as a string: it is not escaped a
    /// second time, and the `[limits.log_field]` cap does not apply to it. A
    /// nested document is bounded by the line's own buffer,
    /// `[limits.log_buffer] max`, because a nested value is not a string field
    /// and must not be cut at 8 KiB: a document cut in half is not JSON.
    ///
    /// `serde_json` writes field by field into the Sink's buffer, so the value
    /// costs no `String` and no intermediate document. It does allocate what
    /// the value itself allocates before this call, which is the caller's to
    /// hold and reuse.
    ///
    /// A value the buffer cannot hold refuses the whole line, exactly as a
    /// string field past its cap does.
    pub fn json<T: serde::Serialize + ?Sized>(self, key: &str, value: &T) -> Self {
        let mut line = self.field_head(key);
        if line.failed {
            return line;
        }
        let failed = {
            let mut adapter = JsonAdapter {
                sink: line.sink,
                failed: false,
            };
            let result = serde_json::to_writer(&mut adapter, value);
            adapter.failed || result.is_err()
        };
        if failed {
            line.failed = true;
        }
        line
    }

    /// Add a field through `Display`, formatted into the buffer with no
    /// `String`.
    pub fn display(self, key: &str, value: &dyn fmt::Display) -> Self {
        self.field_head(key)
            .formatted_body(format_args!("{}", value))
    }

    /// Add a field through `Debug`, formatted into the buffer with no `String`.
    pub fn debug(self, key: &str, value: &dyn fmt::Debug) -> Self {
        self.field_head(key)
            .formatted_body(format_args!("{:?}", value))
    }

    /// Add a field whose JSON type its Rust type decides. The macros'
    /// `key = value` form uses this.
    pub fn field<V: Field>(self, key: &str, value: V) -> Self {
        value.record(self, key)
    }

    /// Add the message when it is not known at [`Line::begin`], as the macros
    /// write it after their fields.
    pub fn message(self, args: fmt::Arguments<'_>) -> Self {
        self.field_head("message").formatted_body(args)
    }

    /// Close the line, commit it to the Sink's buffer, and let the Sink drop it
    /// if it still does not fit.
    pub fn end(mut self) {
        if !self.failed {
            self = self.raw(b"}");
            if let Some(target) = self.target {
                self = self.raw(b",\"target\":\"");
                if !self.failed && !self.sink.push_escaped(target) {
                    self.failed = true;
                }
                self = self.raw(b"\"");
            }
            let kind = self.kind;
            self = self.raw(b",\"type\":\"");
            self = self.raw(kind.text().as_bytes());
            self = self.raw(b"\"");
            self = self.raw(b"}\n");
        }
        if self.failed {
            self.sink.rollback_line();
        } else {
            self.sink.commit_line();
        }
    }

    fn message_str(self, message: &str) -> Self {
        self.field_head("message").quoted(message)
    }

    fn field_head(mut self, key: &str) -> Self {
        if self.failed {
            return self;
        }
        if !self.first {
            self = self.raw(b",");
        }
        self.first = false;
        self = self.raw(b"\"");
        if !self.failed && !self.sink.push_escaped(key) {
            self.failed = true;
        }
        self.raw(b"\":")
    }

    fn quoted(mut self, value: &str) -> Self {
        if self.failed {
            return self;
        }
        if !self.sink.push(b"\"") {
            self.failed = true;
            return self;
        }
        let mut left = self.sink.field_max();
        match self.sink.push_escaped_limited(value, &mut left) {
            Ok(true) => {}
            // Past the maximum the whole line is refused, never cut.
            Ok(false) => {
                self.sink.refuse_field();
                self.failed = true;
                return self;
            }
            Err(()) => {
                self.failed = true;
                return self;
            }
        }
        if !self.sink.push(b"\"") {
            self.failed = true;
        }
        self
    }

    /// Write a string body in quotes, escaped by the adapter. A body past the
    /// `log_field` maximum refuses the line.
    fn formatted_body(mut self, args: fmt::Arguments<'_>) -> Self {
        if self.failed {
            return self;
        }
        if !self.sink.push(b"\"") {
            self.failed = true;
            return self;
        }
        let (cut, failed) = {
            let left = self.sink.field_max();
            let mut adapter = EscapeAdapter {
                sink: self.sink,
                left,
                cut: false,
                failed: false,
            };
            let result = fmt::write(&mut adapter, args);
            (adapter.cut, adapter.failed || result.is_err())
        };
        if cut {
            self.sink.refuse_field();
        }
        if failed || cut {
            self.failed = true;
            return self;
        }
        if !self.sink.push(b"\"") {
            self.failed = true;
        }
        self
    }

    fn number<T: itoa::Integer>(mut self, key: &str, value: T) -> Self {
        self = self.field_head(key);
        let mut buffer = itoa::Buffer::new();
        self.raw(buffer.format(value).as_bytes())
    }

    fn raw(mut self, bytes: &[u8]) -> Self {
        if !self.failed && !self.sink.push(bytes) {
            self.failed = true;
        }
        self
    }
}

/// A `fmt::Write` that escapes into the line's buffer and cuts on a character
/// boundary at the escaped field limit.
struct EscapeAdapter<'a> {
    sink: &'a mut Sink,
    left: usize,
    cut: bool,
    failed: bool,
}

impl fmt::Write for EscapeAdapter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.failed {
            return Err(fmt::Error);
        }
        if self.cut {
            return Ok(());
        }
        match self.sink.push_escaped_limited(s, &mut self.left) {
            Ok(true) => Ok(()),
            Ok(false) => {
                self.cut = true;
                self.left = 0;
                Ok(())
            }
            Err(()) => {
                self.failed = true;
                Err(fmt::Error)
            }
        }
    }
}

/// An `io::Write` over the line's buffer, for `serde_json` to write a nested
/// value into.
///
/// No escaping: `serde_json` writes JSON, and escaping it again would turn the
/// document into a string. A write the buffer will not take fails rather than
/// waits, which marks the line refused and rolls it back whole.
struct JsonAdapter<'a> {
    sink: &'a mut Sink,
    failed: bool,
}

impl std::io::Write for JsonAdapter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.sink.push(buf) {
            Ok(buf.len())
        } else {
            self.failed = true;
            Err(std::io::ErrorKind::WriteZero.into())
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ── Typed fields for the macros ─────────────────────────────────────────────

/// The JSON type of a `key = value` field. Hidden: it is the macros' dispatch,
/// not an interface to write against.
#[doc(hidden)]
pub trait Field {
    /// Append the field to `line`.
    fn record<'a>(self, line: Line<'a>, key: &str) -> Line<'a>;
}

impl Field for &str {
    fn record<'a>(self, line: Line<'a>, key: &str) -> Line<'a> {
        line.str(key, self)
    }
}

impl Field for String {
    fn record<'a>(self, line: Line<'a>, key: &str) -> Line<'a> {
        line.str(key, &self)
    }
}

impl Field for &String {
    fn record<'a>(self, line: Line<'a>, key: &str) -> Line<'a> {
        line.str(key, self)
    }
}

impl Field for bool {
    fn record<'a>(self, line: Line<'a>, key: &str) -> Line<'a> {
        line.bool(key, self)
    }
}

macro_rules! field_integer {
    ($($t:ty),*) => {
        $(impl Field for $t {
            fn record<'a>(self, line: Line<'a>, key: &str) -> Line<'a> {
                line.number(key, self)
            }
        })*
    };
}

field_integer!(u8, u16, u32, u64, usize, u128, i8, i16, i32, i64, isize, i128);

impl<T: Field> Field for Option<T> {
    fn record<'a>(self, line: Line<'a>, key: &str) -> Line<'a> {
        match self {
            Some(value) => value.record(line, key),
            None => line,
        }
    }
}

/// Write the line that reports how many lines a full destination lost.
pub(crate) fn write_dropped_line(sink: &mut Sink, dropped: u64) {
    Line::begin(sink, Level::Warn, "log lines dropped")
        .u64("dropped", dropped)
        .target("m6_log")
        .end();
}

/// [`Line::begin`] with the line's time injected, for tests.
#[doc(hidden)]
pub fn line_at<'a>(now: SystemTime, sink: &'a mut Sink, level: Level, message: &str) -> Line<'a> {
    Line::open(sink, level, now).message_str(message)
}
