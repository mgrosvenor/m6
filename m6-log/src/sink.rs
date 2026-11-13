//! One destination: a buffer allocated at start and the descriptor it drains to.
//!
//! A Sink holds complete lines at the front of its buffer (`..commit`) and the
//! line currently being built after them (`commit..len`). `flush` writes only
//! the complete lines, so a write never splits a line and a flush during a line
//! keeps the partial line at the front of the buffer.

use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal};
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, OFlag};

use crate::line::write_dropped_line;

/// Bytes one Sink's buffer holds before it is written to its destination.
pub const BUFFER_BYTES: usize = 16 * 1024;

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
    /// The buffer, made once at start.
    buf: Box<[u8]>,
    /// Bytes in `buf`, including the line being built.
    len: usize,
    /// Bytes of `buf` that are complete lines and may be written.
    commit: usize,
    /// The descriptor to write to.
    fd: RawFd,
    /// The file behind `fd`, when this sink is a file.
    file: Option<File>,
    /// The absolute path `file` was opened at, for [`Sink::reopen`].
    path: Option<PathBuf>,
    /// The flags `fd` had before `Sink::stdout` changed them, restored by
    /// [`Sink::finish`].
    original_flags: Option<OFlag>,
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
            let flags = fcntl(std::io::stdout(), FcntlArg::F_GETFL).map_err(io::Error::from)?;
            let flags = OFlag::from_bits_truncate(flags);
            fcntl(
                std::io::stdout(),
                FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK),
            )
            .map_err(io::Error::from)?;
            Some(flags)
        };
        Ok(Sink {
            buf: vec![0; BUFFER_BYTES].into_boxed_slice(),
            len: 0,
            commit: 0,
            fd: 1,
            file: None,
            path: None,
            original_flags: original,
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
    /// A regular file never returns `EAGAIN`, so a file sink is never
    /// registered with a loop.
    pub fn file(path: &Path) -> io::Result<Sink> {
        let path = std::path::absolute(path)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let fd = file.as_raw_fd();
        Ok(Sink {
            buf: vec![0; BUFFER_BYTES].into_boxed_slice(),
            len: 0,
            commit: 0,
            fd,
            file: Some(file),
            path: Some(path),
            original_flags: None,
            blocked: false,
            dropped: 0,
            reported: 0,
            report: 0,
            report_upto: 0,
            lines: 0,
            written: 0,
        })
    }

    /// The descriptor to watch for writability while [`Sink::wants_write`].
    pub fn fd(&self) -> RawFd {
        self.fd
    }

    /// Bytes are waiting on a destination that could not take them.
    pub fn wants_write(&self) -> bool {
        self.commit > 0
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

    /// Flush, then open the file at its path afresh.
    ///
    /// The open happens on the analytics file at SIGUSR1, so a rotation tool's
    /// rename takes effect for the next line. A standard-output sink has no
    /// path and does nothing.
    pub fn reopen(&mut self) -> io::Result<()> {
        self.flush();
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        self.fd = file.as_raw_fd();
        self.file = Some(file);
        Ok(())
    }

    /// The last word, at stop: restore the flags `Sink::stdout` changed, write
    /// everything that is buffered, then the dropped-count line.
    ///
    /// A restored blocking descriptor can wait here, which is what stop may do.
    pub fn finish(&mut self) -> io::Result<()> {
        if let Some(flags) = self.original_flags.take() {
            fcntl(std::io::stdout(), FcntlArg::F_SETFL(flags)).map_err(io::Error::from)?;
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
        if self.len + bytes.len() > self.buf.len() {
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
                Err(Errno::EINTR) => continue,
                Err(Errno::EAGAIN) => {
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
                Err(Errno::EINTR) => continue,
                Err(e) => {
                    self.discard_complete();
                    self.report = 0;
                    return Err(io::Error::from(e));
                }
            }
        }
        Ok(())
    }

    fn write_some(&self, bytes: &[u8]) -> Result<usize, Errno> {
        match self.file.as_ref() {
            Some(file) => nix::unistd::write(file, bytes),
            None => nix::unistd::write(std::io::stdout(), bytes),
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
