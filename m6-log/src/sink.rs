//! One destination: a buffer allocated at start and the descriptor it drains to.
//!
//! A Sink holds complete lines at the front of its buffer (`..commit`) and the
//! line currently being built after them (`commit..len`). `flush` writes only
//! the complete lines, so a write never splits a line and a flush during a line
//! keeps the partial line at the front of the buffer.

use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal};
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;

use crate::journal::{Journal, Outcome, DATAGRAM_MAX};
use crate::line::write_dropped_line;

/// The default size of one Sink's buffer at start, in bytes.
pub const BUFFER_BYTES: usize = 16 * 1024;

/// The default most a Sink's buffer grows to, in bytes.
pub const BUFFER_MAX_BYTES: usize = 1024 * 1024;

/// The default longest string field of a line, in bytes.
pub const FIELD_MAX_BYTES: usize = 8 * 1024;

/// How big the Sinks made after this call are, and how long a field may be.
///
/// A Sink's buffer starts at `buffer_start`, grows by doubling when a line does
/// not fit and the destination cannot take it, keeps its size, and refuses a
/// line past `buffer_max`. A string field longer than `field_max` refuses its
/// line. Both refusals write one line to stderr that names the limit and
/// `file`, the config file that sets it. The service sets these from its
/// `[limits]` table before it opens the log.
#[derive(Debug, Clone)]
pub struct LogLimits {
    pub buffer_start: usize,
    pub buffer_max: usize,
    pub field_max: usize,
    /// The config file the limits came from, for the refusal message.
    pub file: String,
}

impl Default for LogLimits {
    fn default() -> Self {
        Self {
            buffer_start: BUFFER_BYTES,
            buffer_max: BUFFER_MAX_BYTES,
            field_max: FIELD_MAX_BYTES,
            file: "the config file".to_string(),
        }
    }
}

impl LogLimits {
    /// The error for a line that needs `needed` bytes of buffer.
    pub fn buffer_refusal(&self, needed: usize) -> String {
        format!(
            "m6-log: the log buffer needs at least {needed} B, over [limits.log_buffer] max = {} in {}; raise max to fix this. A log line was refused.",
            kib(self.buffer_max),
            self.file
        )
    }

    /// The error for a line whose journal datagram is over the socket's limit.
    pub fn datagram_refusal(&self, needed: usize) -> String {
        format!(
            "m6-log: a log line needs a {needed} B journal datagram, over the {DATAGRAM_MAX} B socket limit; the line was refused and counted as dropped. Shorten the line."
        )
    }

    /// The error for a string field longer than `field_max`.
    pub fn field_refusal(&self) -> String {
        format!(
            "m6-log: a log field needs at least {} B, over [limits.log_field] max = {} in {}; raise max to fix this. A log line was refused.",
            self.field_max + 1,
            kib(self.field_max),
            self.file
        )
    }
}

static LIMITS: std::sync::Mutex<Option<LogLimits>> = std::sync::Mutex::new(None);

/// Set the limits of every Sink made after this call. Start only.
pub fn set_limits(limits: LogLimits) {
    *LIMITS.lock().unwrap_or_else(|e| e.into_inner()) = Some(limits);
}

fn current_limits() -> LogLimits {
    LIMITS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default()
}

fn kib(bytes: usize) -> String {
    if bytes.is_multiple_of(1024 * 1024) {
        format!("{} MiB", bytes / (1024 * 1024))
    } else if bytes.is_multiple_of(1024) {
        format!("{} KiB", bytes / 1024)
    } else {
        format!("{bytes} B")
    }
}

/// The escaped form of one byte of a JSON string's contents, and its length.
///
/// This is the one escaper: a match over the byte, with no table. Bytes above
/// 0x1f that are not `"` or `\` are copied through, so the non-ASCII bytes of
/// a `str`'s UTF-8 pass unchanged.
fn escaped_byte(b: u8) -> ([u8; 6], usize) {
    let mut out = [0u8; 6];
    let len = match b {
        b'"' => {
            out[..2].copy_from_slice(b"\\\"");
            2
        }
        b'\\' => {
            out[..2].copy_from_slice(b"\\\\");
            2
        }
        0x00..=0x1f => {
            let hex = b"0123456789abcdef";
            out.copy_from_slice(&[
                b'\\',
                b'u',
                b'0',
                b'0',
                hex[(b >> 4) as usize],
                hex[(b & 0x0f) as usize],
            ]);
            6
        }
        _ => {
            out[0] = b;
            1
        }
    };
    (out, len)
}

/// One log destination: a fixed buffer, its length and the descriptor it
/// drains to.
pub struct Sink {
    /// The buffer: `buf.len()` is the size it has grown to, never smaller than
    /// at start.
    buf: Vec<u8>,
    limits: LogLimits,
    /// A refusal has been written to stderr since the buffer last drained.
    refusal_noted: bool,
    /// Bytes in `buf`, including the line being built.
    len: usize,
    /// Bytes of `buf` that are complete lines and may be written.
    commit: usize,
    /// The descriptor to write to.
    fd: RawFd,
    /// The file behind `fd`, when this sink is a file.
    file: Option<File>,
    /// The journal's socket, when this process writes native journal fields.
    /// A failed send clears it, and the bytes go to stdout from then on.
    journal: Option<Journal>,
    /// The flags `fd` had before `Sink::stdout` changed them, restored by
    /// [`Sink::finish`].
    original_nonblocking: Option<bool>,
    /// The last write could not take the bytes and the destination is full.
    blocked: bool,
    /// Lines lost to a full destination or a write error, ever.
    dropped: u64,
    /// Dropped lines the last drained dropped-count line reported.
    reported: u64,
    /// Bytes of a dropped-count line still at the front of the buffer.
    report: usize,
    /// The `dropped` total that the buffered dropped-count line reports.
    report_upto: u64,
    /// Lines this sink has accepted.
    lines: u64,
    /// Bytes this sink has written to its destination.
    written: u64,
}

impl Sink {
    /// A sink on standard output.
    ///
    /// `O_NONBLOCK` is turned on unless stdout is a terminal, because the flag
    /// belongs to the open file and would outlive the process in the
    /// developer's shell. [`Sink::finish`] restores the flags found here.
    pub fn stdout() -> io::Result<Sink> {
        let terminal = std::io::stdout().is_terminal();
        let original = if terminal {
            None
        } else {
            Some(m6_platform::fd::set_nonblocking(&std::io::stdout(), true)?)
        };
        Ok(Sink {
            buf: vec![0; current_limits().buffer_start],
            limits: current_limits(),
            refusal_noted: false,
            len: 0,
            commit: 0,
            fd: 1,
            file: None,
            journal: None,
            original_nonblocking: original,
            blocked: false,
            dropped: 0,
            reported: 0,
            report: 0,
            report_upto: 0,
            lines: 0,
            written: 0,
        })
    }

    /// A sink on a file opened for append.
    ///
    /// The directory the file goes in must already exist. A missing directory
    /// is refused rather than created, so a misconfigured path stops the
    /// service at start instead of putting data where the operator did not
    /// intend. A regular file never returns `EAGAIN`, so a file sink is never
    /// registered with a loop.
    ///
    /// The error names the directory, the user who should own it and the
    /// remedy. [`crate::init`] writes nothing but stdout.
    pub fn file(path: &Path) -> io::Result<Sink> {
        Sink::file_setting(path, "the log file path")
    }

    /// A sink on a file whose path was named by `setting`.
    ///
    /// `setting` is the name a reader can act on, so a missing directory's
    /// error says what to fix as well as what to create.
    pub(crate) fn file_setting(path: &Path, setting: &str) -> io::Result<Sink> {
        let path = std::path::absolute(path)?;
        if let Some(parent) = path.parent() {
            if !parent.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "{setting}: the directory {} does not exist: create it, owned by {}, or fix {setting}",
                        parent.display(),
                        m6_platform::process::user()
                    ),
                ));
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let fd = file.as_raw_fd();
        Ok(Sink {
            buf: vec![0; current_limits().buffer_start],
            limits: current_limits(),
            refusal_noted: false,
            len: 0,
            commit: 0,
            fd,
            file: Some(file),
            journal: None,
            original_nonblocking: None,
            blocked: false,
            dropped: 0,
            reported: 0,
            report: 0,
            report_upto: 0,
            lines: 0,
            written: 0,
        })
    }

    /// Write the lines to the journal's native socket, one datagram each.
    ///
    /// Returns the sink, with the journal attached, when this process is
    /// under systemd. stdout keeps the flags [`Sink::stdout`] set, because a
    /// failed send falls back to it in the same process.
    pub(crate) fn with_journal(mut self, journal: Journal) -> Sink {
        self.journal = Some(journal);
        self
    }

    /// The descriptor to watch for writability while [`Sink::wants_write`].
    pub fn fd(&self) -> RawFd {
        self.fd
    }

    /// Bytes are waiting on a destination that could not take them.
    ///
    /// True only after a write returned `EAGAIN`, not merely because complete
    /// lines are buffered: a line waiting for the tick is not a reason to ask
    /// the reactor for a writable edge. A regular file and a terminal never
    /// return `EAGAIN`, so they are never registered.
    pub fn wants_write(&self) -> bool {
        self.blocked
    }

    /// Write the complete lines, never waiting on a full destination.
    ///
    /// One call writes at most once, and once more for a dropped-count line
    /// when this call drained the destination and lines were lost since the
    /// last report.
    pub fn flush(&mut self) {
        self.try_write();
        if self.commit == 0 && self.len == 0 && self.report == 0 && self.dropped > self.reported {
            let lost = self.dropped - self.reported;
            self.report_upto = self.dropped;
            write_dropped_line(self, lost);
            self.report = self.commit;
            self.try_write();
        }
    }

    /// The last word, at stop: restore the flags `Sink::stdout` changed, write
    /// everything that is buffered, then the dropped-count line.
    ///
    /// A restored blocking descriptor can wait here, which is what stop may do.
    pub fn finish(&mut self) -> io::Result<()> {
        if let Some(was) = self.original_nonblocking.take() {
            m6_platform::fd::set_nonblocking(&std::io::stdout(), was)?;
        }
        if self.journal.is_some() {
            // One non-blocking pass. A line the journal would not take is
            // written to stdout below, where blocking is allowed.
            self.try_write();
            self.journal = None;
        }
        let mut result = self.write_all_blocking();
        if result.is_ok() && self.dropped > self.reported {
            let lost = self.dropped - self.reported;
            self.report_upto = self.dropped;
            write_dropped_line(self, lost);
            self.report = self.commit;
            result = self.write_all_blocking();
        }
        result
    }

    /// Lines lost to a full destination or a write error, ever.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Lines this sink has accepted.
    pub fn lines(&self) -> u64 {
        self.lines
    }

    /// Bytes this sink has written to its destination.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Append one complete line's bytes, flushing complete lines first when it
    /// would not otherwise fit. Returns false when the line cannot fit.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> bool {
        if self.len + bytes.len() > self.buf.len() {
            self.make_room();
        }
        if self.len + bytes.len() > self.buf.len() && !self.grow(self.len + bytes.len()) {
            return false;
        }
        self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
        true
    }

    /// Append a string, escaping it as a JSON string's contents.
    ///
    /// The printable-ASCII fast path stays. Every other byte goes through
    /// [`escaped_byte`], the one escaper.
    pub(crate) fn push_escaped(&mut self, s: &str) -> bool {
        if s.bytes()
            .all(|b| (0x20..0x7f).contains(&b) && b != b'"' && b != b'\\')
        {
            return self.push(s.as_bytes());
        }
        for &b in s.as_bytes() {
            let (escaped, len) = escaped_byte(b);
            if !self.push(&escaped[..len]) {
                return false;
            }
        }
        true
    }

    /// Append as much of `s` as fits in `left` more escaped bytes, on a
    /// character boundary. Answers whether the whole value was written and
    /// reduces `left` by the bytes written. `Err` means the buffer itself is
    /// full.
    #[inline]
    pub(crate) fn push_escaped_limited(&mut self, s: &str, left: &mut usize) -> Result<bool, ()> {
        if s.len() <= *left
            && s.bytes()
                .all(|b| (0x20..0x7f).contains(&b) && b != b'"' && b != b'\\')
        {
            return if self.push(s.as_bytes()) {
                *left -= s.len();
                Ok(true)
            } else {
                Err(())
            };
        }
        for c in s.chars() {
            let mut utf8 = [0u8; 4];
            let text = c.encode_utf8(&mut utf8);
            let mut escaped = [0u8; 6];
            let mut len = 0;
            for &b in text.as_bytes() {
                let (bytes, n) = escaped_byte(b);
                escaped[len..len + n].copy_from_slice(&bytes[..n]);
                len += n;
            }
            if len > *left {
                return Ok(false);
            }
            if !self.push(&escaped[..len]) {
                return Err(());
            }
            *left -= len;
        }
        Ok(true)
    }

    pub(crate) fn commit_line(&mut self) {
        self.commit = self.len;
        self.lines += 1;
    }

    pub(crate) fn rollback_line(&mut self) {
        self.len = self.commit;
        self.dropped += 1;
    }

    /// Grow the buffer to hold `needed` bytes: doubling, clamped to the
    /// maximum. Past the maximum the line is refused and one line on stderr
    /// says which limit to raise.
    fn grow(&mut self, needed: usize) -> bool {
        let max = self.limits.buffer_max;
        if needed > max {
            if !self.refusal_noted {
                self.refusal_noted = true;
                eprintln!("{}", self.limits.buffer_refusal(needed));
            }
            return false;
        }
        let from = self.buf.len();
        let mut size = from;
        while size < needed {
            size = size.saturating_mul(2);
        }
        let size = size.min(max);
        self.buf.resize(size, 0);
        // Not through the log itself: the line would be written into the
        // buffer that is growing.
        eprintln!(
            "m6-log: limit log_buffer grew from {} to {} for a log line (max {}, [limits.log_buffer] in {})",
            kib(from),
            kib(size),
            kib(max),
            self.limits.file
        );
        true
    }

    /// The size the buffer has grown to, in bytes.
    pub fn buffer_size(&self) -> usize {
        self.buf.len()
    }

    /// The longest string field a line of this Sink may carry.
    pub(crate) fn field_max(&self) -> usize {
        self.limits.field_max
    }

    /// A string field was longer than the `log_field` maximum: its line is
    /// refused, and one line on stderr says which limit to raise.
    pub(crate) fn refuse_field(&mut self) {
        eprintln!("{}", self.limits.field_refusal());
    }

    /// Make room for more line bytes: write complete lines and move the line
    /// being built to the front. Does nothing while the destination is known
    /// to be full: only an explicit flush retries then.
    fn make_room(&mut self) {
        if !self.blocked {
            self.try_write();
        }
    }

    /// One write pass over the complete lines. A short write keeps the rest
    /// buffered. A failure other than `EAGAIN` counts the complete lines it
    /// could not send as dropped.
    fn try_write(&mut self) {
        if self.commit == 0 {
            self.blocked = false;
            return;
        }
        if self.journal.is_some() && self.write_journal() {
            return;
        }
        let mut written = 0;
        let mut failed = false;
        let mut full = false;
        loop {
            if written == self.commit {
                break;
            }
            match self.write_some(&self.buf[written..self.commit]) {
                Ok(0) => {
                    failed = true;
                    break;
                }
                Ok(n) => written += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    full = true;
                    break;
                }
                Err(_) => {
                    failed = true;
                    break;
                }
            }
        }
        let report = self.report;
        self.move_front(written);
        self.advance_report(report, written);
        if failed {
            self.discard_complete();
            self.report = 0;
            self.blocked = false;
        } else if full {
            self.blocked = true;
        } else if self.commit == 0 {
            self.blocked = false;
        }
        if self.commit == 0 {
            self.refusal_noted = false;
        }
    }

    /// One pass over the complete lines as journal datagrams.
    ///
    /// Answers true when the journal handled the pass: every complete line
    /// left the buffer, or the journal's queue was full and the rest stay
    /// buffered for the next flush. The socket is not watched for
    /// writability, so the loop's next flush or a later line retries. Answers
    /// false when a send failed for good, after switching the sink to stdout:
    /// the lines that were not sent are still buffered, and the caller writes
    /// them to stdout as bytes.
    fn write_journal(&mut self) -> bool {
        let mut sent = 0;
        let mut lost = 0u64;
        let mut refused = 0usize;
        while sent < self.commit {
            let Some(end) = self.buf[sent..self.commit]
                .iter()
                .position(|&byte| byte == b'\n')
                .map(|at| sent + at + 1)
            else {
                break;
            };
            let outcome = {
                let journal = self
                    .journal
                    .as_mut()
                    .expect("write_journal runs with a journal");
                journal.send(&self.buf[sent..end - 1])
            };
            match outcome {
                Outcome::Sent => sent = end,
                Outcome::Refused { needed } => {
                    lost += 1;
                    refused = needed;
                    sent = end;
                }
                Outcome::Full => break,
                Outcome::Failed => {
                    // The same bytes go to stdout, which under systemd is the
                    // same journal. Nothing is dropped and nothing is sent
                    // twice: the lines already sent leave the buffer here.
                    self.move_front(sent);
                    self.dropped += lost;
                    self.journal = None;
                    self.note_datagram_refusal(refused);
                    return false;
                }
            }
        }
        self.move_front(sent);
        self.dropped += lost;
        self.note_datagram_refusal(refused);
        self.blocked = false;
        true
    }

    /// One line on stderr the first time a datagram is too long, naming the
    /// limit, as an oversized buffer names its own.
    fn note_datagram_refusal(&mut self, needed: usize) {
        if needed == 0 || self.refusal_noted {
            return;
        }
        self.refusal_noted = true;
        eprintln!("{}", self.limits.datagram_refusal(needed));
    }

    /// Advance `reported` once the bytes of a dropped-count line have drained.
    fn advance_report(&mut self, report: usize, written: usize) {
        if report == 0 {
            return;
        }
        if written >= report {
            self.reported = self.report_upto;
            self.report = 0;
            self.report_upto = 0;
        } else {
            self.report = report - written;
        }
    }

    /// Blocking write of everything buffered, for [`Sink::finish`].
    fn write_all_blocking(&mut self) -> io::Result<()> {
        while self.commit > 0 {
            let report = self.report;
            match self.write_some(&self.buf[..self.commit]) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
                Ok(n) => {
                    self.move_front(n);
                    self.advance_report(report, n);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.discard_complete();
                    self.report = 0;
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    fn write_some(&self, bytes: &[u8]) -> io::Result<usize> {
        match self.file.as_ref() {
            Some(file) => m6_platform::fd::write(file, bytes),
            None => m6_platform::fd::write(&std::io::stdout(), bytes),
        }
    }

    /// Remove the first `written` bytes, keeping the line being built.
    fn move_front(&mut self, written: usize) {
        if written == 0 {
            return;
        }
        self.written += written as u64;
        self.buf.copy_within(written..self.len, 0);
        self.len -= written;
        self.commit -= written;
    }

    /// Throw away the complete lines that could not be written, keeping the
    /// line being built, and count them as dropped.
    fn discard_complete(&mut self) {
        let lost = self.buf[self.report..self.commit]
            .iter()
            .filter(|&&b| b == b'\n')
            .count() as u64;
        self.dropped += lost;
        self.buf.copy_within(self.commit..self.len, 0);
        self.len -= self.commit;
        self.commit = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::line::{Level, Line};
    use std::os::unix::net::UnixDatagram;

    /// A file sink with a journal on one end of a datagram pair, the other
    /// end returned for the test to read, and the file's path.
    fn journal_sink(dir: &Path) -> (Sink, UnixDatagram, std::path::PathBuf) {
        let (sender, receiver) = UnixDatagram::pair().expect("a datagram pair");
        sender.set_nonblocking(true).expect("non-blocking");
        receiver.set_nonblocking(true).expect("non-blocking");
        let path = dir.join("stdout.log");
        let sink = Sink::file(&path)
            .expect("the file sink")
            .with_journal(Journal::on(sender));
        (sink, receiver, path)
    }

    /// Every datagram waiting on `receiver`.
    fn drain(receiver: &UnixDatagram) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; DATAGRAM_MAX];
        while let Ok(n) = receiver.recv(&mut buf) {
            out.push(buf[..n].to_vec());
        }
        out
    }

    /// A full journal queue is a wait, never a failure. The sink once
    /// treated the first `EAGAIN` as a failed send and wrote to stdout for
    /// the rest of the process.
    #[test]
    fn a_full_journal_keeps_the_lines_and_the_journal() {
        let dir = tempfile::tempdir().expect("a directory");
        let (mut sink, receiver, path) = journal_sink(dir.path());
        let lines = 3000;
        for i in 0..lines {
            Line::begin(&mut sink, Level::Info, "a line")
                .str("n", &i.to_string())
                .end();
            sink.flush();
        }
        assert!(
            sink.commit > 0,
            "the test never filled the journal's queue, so it measured nothing"
        );
        assert!(
            sink.journal.is_some(),
            "a full queue switched the sink to stdout"
        );
        let mut received = drain(&receiver).len();
        while sink.commit > 0 {
            sink.flush();
            received += drain(&receiver).len();
        }
        assert_eq!(received, lines, "every line reached the journal once");
        assert_eq!(sink.dropped(), 0);
        let file = std::fs::read(&path).expect("the stdout file");
        assert!(file.is_empty(), "{} bytes went to stdout", file.len());
    }
}
