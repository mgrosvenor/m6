//! The native journal write: one datagram per line, no libsystemd.
//!
//! Under systemd, stdout is the journal. journald also offers its own socket,
//! and a line written there carries fields a reader filters on without
//! parsing JSON: `PRIORITY` from the level, `M6_TYPE` from the line's `type`,
//! `SYSLOG_IDENTIFIER` from the program's name, and `MESSAGE` with the whole
//! JSON line.
//!
//! The wire format is the one `sd_journal_send` uses, written here directly:
//! the fields of one datagram, each ending with a newline, and a value that
//! contains a newline written in the binary form, which is the field name, a
//! newline, an eight-byte little-endian length, the value and a newline. That
//! format has needed no library since it was fixed, which is why m6 links
//! nothing for it.
//!
//! A line longer than [`DATAGRAM_MAX`] is refused the way a line longer than
//! the buffer is refused: it counts as dropped and one line on stderr names
//! the limit. `sd_journal_send` sends such a message through a `memfd`
//! instead. m6 refuses it, because a refused line is visible in the dropped
//! count and a second transport is not.
//!
//! A send that would block keeps the line buffered, and the next flush sends
//! it again. A send that fails for any other reason, such as a socket that is
//! gone, leaves the line to stdout, which under systemd is the same journal.
//! The Sink then switches to stdout for the rest of the process, so no line is
//! dropped and none is sent twice.

use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;

use crate::line::{Level, Type};

/// The journal's native socket for a system service.
const SYSTEM_SOCKET: &str = "/run/systemd/journal/socket";

/// The longest datagram this module sends, in bytes.
///
/// journald accepts a datagram up to the receiving socket's buffer, and a
/// unix datagram above the sender's buffer fails with `EMSGSIZE`. 64 KiB is
/// the size both carry on every supported kernel, so a line that needs more
/// is refused rather than gambled on a larger buffer.
pub const DATAGRAM_MAX: usize = 64 * 1024;

/// The socket a test's own journal receiver is bound at, when it sets one.
///
/// A test cannot own `/run/systemd/journal/socket`, so it binds its own
/// datagram socket and names it here. A production process never sets it.
const TEST_SOCKET: &str = "M6_TEST_JOURNAL_SOCKET";

/// The outcome of one line's send.
pub(crate) enum Outcome {
    /// The line is one datagram and the journal took it.
    Sent,
    /// The datagram needed `needed` bytes, over [`DATAGRAM_MAX`]. The line is
    /// refused.
    Refused { needed: usize },
    /// The journal's queue is full. The line stays buffered for the next
    /// flush, as a line on a full stdout does.
    Full,
    /// The send failed for good: the socket is gone or refuses. The line goes
    /// to stdout.
    Failed,
}

/// The journal's socket, the program's name and the datagram being built.
pub(crate) struct Journal {
    socket: UnixDatagram,
    identifier: String,
    datagram: Vec<u8>,
}

impl Journal {
    /// A journal on an already connected, non-blocking socket, for a test.
    #[cfg(test)]
    pub(crate) fn on(socket: UnixDatagram) -> Journal {
        Journal {
            socket,
            identifier: "m6-test".to_string(),
            datagram: Vec::with_capacity(DATAGRAM_MAX),
        }
    }

    /// The open journal socket, or `None` when this process does not write to
    /// the journal.
    ///
    /// `None` covers a process outside systemd and a socket that cannot be
    /// opened. The second prints one line on stderr, because an operator who
    /// expected native fields needs to know they are missing.
    pub(crate) fn open() -> Option<Journal> {
        let path = socket_path()?;
        let socket = match UnixDatagram::unbound().and_then(|socket| {
            socket.connect(&path)?;
            Ok(socket)
        }) {
            Ok(socket) => socket,
            Err(error) => {
                eprintln!(
                    "m6-log: the journal socket at {} could not be opened: {error}; writing to stdout",
                    path.display()
                );
                return None;
            }
        };
        // The loop must never wait on a full socket, the same rule stdout
        // follows. A send that would block keeps the line buffered for the
        // next flush.
        if let Err(error) = socket.set_nonblocking(true) {
            eprintln!(
                "m6-log: the journal socket at {} could not be made non-blocking: {error}; writing to stdout",
                path.display()
            );
            return None;
        }
        Some(Journal {
            socket,
            identifier: identifier(),
            datagram: Vec::with_capacity(DATAGRAM_MAX),
        })
    }

    /// Send one line, whose bytes are the JSON object without its newline.
    pub(crate) fn send(&mut self, line: &[u8]) -> Outcome {
        let (level, kind) = header(line);
        self.datagram.clear();
        let fields: [(&[u8], &[u8]); 4] = [
            (b"PRIORITY", priority(level)),
            (b"M6_TYPE", kind.text().as_bytes()),
            (b"SYSLOG_IDENTIFIER", self.identifier.as_bytes()),
            (b"MESSAGE", line),
        ];
        for (name, value) in fields.iter() {
            push_field(&mut self.datagram, name, value);
        }
        if self.datagram.len() > DATAGRAM_MAX {
            return Outcome::Refused {
                needed: self.datagram.len(),
            };
        }
        match self.socket.send(&self.datagram) {
            Ok(sent) if sent == self.datagram.len() => Outcome::Sent,
            Err(error) if m6_platform::fd::send_would_retry(&error) => Outcome::Full,
            _ => Outcome::Failed,
        }
    }
}

/// The socket to write to, or `None` when the journal is not the destination.
fn socket_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(TEST_SOCKET) {
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    if !m6_platform::journal_stream() {
        return None;
    }
    // A user service keeps its journal socket under the runtime directory,
    // and a system service under /run.
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        let user = PathBuf::from(runtime).join("systemd/journal/socket");
        if user.exists() {
            return Some(user);
        }
    }
    Some(PathBuf::from(SYSTEM_SOCKET))
}

/// `SYSLOG_IDENTIFIER`: the program's own name, as journald would fill it in
/// when the field is absent.
fn identifier() -> String {
    std::env::args_os()
        .next()
        .map(PathBuf::from)
        .and_then(|argv| {
            argv.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "m6".to_string())
}

/// Append one field, terminated by a newline, as journald reads it.
///
/// A value with no newline is the text form, `NAME=value\n`. A value with a
/// newline is the binary form, `NAME\n`, an eight-byte little-endian length,
/// the value and `\n`, because the text form cannot carry a newline and
/// journald does not decode a backslash escape. Every field ends with the
/// newline, including the last, because journald discards an unterminated
/// last field.
fn push_field(out: &mut Vec<u8>, name: &[u8], value: &[u8]) {
    out.extend_from_slice(name);
    if value.contains(&b'\n') {
        out.push(b'\n');
        out.extend_from_slice(&(value.len() as u64).to_le_bytes());
        out.extend_from_slice(value);
    } else {
        out.push(b'=');
        out.extend_from_slice(value);
    }
    out.push(b'\n');
}

/// The syslog priority of a level. `TRACE` is `DEBUG`, because syslog has no
/// finer step and the line's own `level` keeps the distinction.
fn priority(level: Level) -> &'static [u8] {
    match level {
        Level::Error => b"3",
        Level::Warn => b"4",
        Level::Info => b"6",
        Level::Debug | Level::Trace => b"7",
    }
}

/// The type of one line, read from its last key, and `event` for a line
/// some other writer handed the Sink.
///
/// [`crate::Line::end`] writes `,"type":"<word>"}` and a newline last, so the
/// word is found by walking back from the end. Reading the tail this way
/// cannot be confused by a field value that spells the same key, which is why
/// the scan starts at the end rather than searching forward.
fn kind_of(line: &[u8]) -> Type {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let Some(body) = line.strip_suffix(b"}") else {
        return Type::Event;
    };
    let Some(body) = body.strip_suffix(b"\"") else {
        return Type::Event;
    };
    let Some(quote) = body.iter().rposition(|&byte| byte == b'"') else {
        return Type::Event;
    };
    let (key, word) = (&body[..quote], &body[quote + 1..]);
    if !key.ends_with(b",\"type\":") {
        return Type::Event;
    }
    Type::ALL
        .into_iter()
        .find(|kind| kind.text().as_bytes() == word)
        .unwrap_or(Type::Event)
}

/// The level and the type of one line, read from the bytes m6-log wrote.
///
/// A [`crate::Line`] writes the level at a fixed offset, after the fixed-width
/// timestamp, and the type last, before the closing brace. Both reads are
/// constant work and both fall back to `INFO` and `event` for a line some
/// other writer handed the Sink, which is the zero of each.
fn header(line: &[u8]) -> (Level, Type) {
    /// `{"timestamp":"` is 14 bytes, the timestamp is 27, and `","level":"` is
    /// 11, so the level word starts at 52.
    const LEVEL_AT: usize = 14 + 27 + 11;
    let prefix = b"{\"timestamp\":\"";
    let level = line
        .strip_prefix(prefix.as_slice())
        .and_then(|_| line.get(LEVEL_AT..))
        .and_then(|rest| {
            Level::ALL
                .into_iter()
                .find(|level| rest.starts_with(level.text().as_bytes()))
        })
        .unwrap_or(Level::Info);
    (level, kind_of(line))
}

/// Whether this process would write to the journal, for a test that has to
/// say what it measured.
#[doc(hidden)]
pub fn under_journal() -> bool {
    socket_path().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_level_and_type_reads_back_from_the_line() {
        for (level, priority) in [
            (Level::Error, b"3".as_slice()),
            (Level::Warn, b"4"),
            (Level::Info, b"6"),
            (Level::Debug, b"7"),
            (Level::Trace, b"7"),
        ] {
            assert_eq!(super::priority(level), priority);
            let line = format!(
                "{{\"timestamp\":\"2026-11-17T18:00:00.000000Z\",\"level\":\"{}\",\"fields\":{{\"message\":\"x\"}},\"type\":\"perf\"}}\n",
                level.text()
            );
            let (read_level, read_type) = header(line.as_bytes());
            assert_eq!(read_level, level);
            assert_eq!(read_type, Type::Perf);
        }
        for (word, kind) in [
            ("event", Type::Event),
            ("perf", Type::Perf),
            ("access", Type::Access),
            ("traffic", Type::Traffic),
            ("startup", Type::Startup),
            ("shutdown", Type::Shutdown),
        ] {
            let line = format!(
                "{{\"timestamp\":\"2026-11-17T18:00:00.000000Z\",\"level\":\"INFO\",\"fields\":{{}},\"type\":\"{word}\"}}\n"
            );
            assert_eq!(header(line.as_bytes()), (Level::Info, kind));
        }
    }

    #[test]
    fn a_field_value_that_names_a_type_does_not_change_the_type() {
        let line = b"{\"timestamp\":\"2026-11-17T18:00:00.000000Z\",\"level\":\"INFO\",\"fields\":{\"path\":\"/a,\\\"type\\\":\\\"perf\\\"}\"},\"type\":\"access\"}\n";
        assert_eq!(header(line), (Level::Info, Type::Access));
    }

    #[test]
    fn a_line_from_another_writer_reads_as_info_and_event() {
        assert_eq!(header(b"not a line\n"), (Level::Info, Type::Event));
        assert_eq!(header(b""), (Level::Info, Type::Event));
    }

    #[test]
    fn every_field_ends_with_a_newline_including_the_last() {
        let mut datagram = Vec::new();
        for (name, value) in [
            (b"PRIORITY".as_slice(), b"6".as_slice()),
            (b"M6_TYPE", b"event"),
            (b"SYSLOG_IDENTIFIER", b"m6"),
            (b"MESSAGE", b"{\"fields\":{}}"),
        ] {
            push_field(&mut datagram, name, value);
        }
        assert_eq!(
            datagram,
            b"PRIORITY=6\nM6_TYPE=event\nSYSLOG_IDENTIFIER=m6\nMESSAGE={\"fields\":{}}\n"
        );
        assert_eq!(
            datagram.last(),
            Some(&b'\n'),
            "the last field must end with a newline or journald discards it"
        );
    }

    #[test]
    fn a_value_with_a_newline_uses_the_length_prefixed_form() {
        let mut out = Vec::new();
        push_field(&mut out, b"MESSAGE", b"a\nb");
        let mut expected = Vec::from(*b"MESSAGE\n");
        expected.extend_from_slice(&3u64.to_le_bytes());
        expected.extend_from_slice(b"a\nb");
        expected.push(b'\n');
        assert_eq!(out, expected);
        assert!(
            !out.windows(2).any(|pair| pair == b"\\n"),
            "a newline in a value is not a backslash and an n: {out:?}"
        );
    }

    #[test]
    fn a_value_without_a_newline_is_the_text_form() {
        let mut out = Vec::new();
        push_field(&mut out, b"PRIORITY", b"4");
        assert_eq!(out, b"PRIORITY=4\n");
    }

    #[test]
    fn the_identifier_is_the_program_name() {
        assert!(!identifier().is_empty());
    }
}
