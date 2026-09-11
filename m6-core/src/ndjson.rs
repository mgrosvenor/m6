//! Newline-delimited JSON, read and written.
//!
//! Core already owns TOML for config. NDJSON is the other format m6 puts on
//! disk: it is what the analytics stream is, and it is the natural shape for
//! anything append-only that a later process reads back. It belongs here for
//! the same reason, and so that no service has to decide for itself what a
//! torn line means.
//!
//! # Torn lines are ordinary, not exceptional
//!
//! An NDJSON file is usually being appended to while it is read. The last line
//! can be half-written, and a reader that fails the whole stream on one bad
//! line will fail whenever it happens to read during a write. So [`read_str`]
//! and [`Reader`] skip what they cannot parse and keep going. When you need to
//! know, [`Reader::skipped`] counts them: silence about a skipped line is fine,
//! but only if it is available to anyone who asks.
//!
//! Blank lines are skipped without counting as errors. A trailing newline is
//! normal and is not a torn record.

use std::io::{BufRead, Write};

use serde::{de::DeserializeOwned, Serialize};

/// Parse every record in a string, skipping blank and unparseable lines.
pub fn read_str<T: DeserializeOwned>(text: &str) -> impl Iterator<Item = T> + '_ {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
}

/// A counting reader over any [`BufRead`].
///
/// Use this over [`read_str`] when the stream is large enough that holding it
/// in memory is wasteful, or when the count of unparseable lines matters.
pub struct Reader<R: BufRead, T> {
    inner: R,
    skipped: usize,
    read: usize,
    buf: String,
    _marker: std::marker::PhantomData<T>,
}

impl<R: BufRead, T: DeserializeOwned> Reader<R, T> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            skipped: 0,
            read: 0,
            buf: String::new(),
            _marker: std::marker::PhantomData,
        }
    }

    /// Lines that were not blank and did not parse.
    pub fn skipped(&self) -> usize {
        self.skipped
    }

    /// Records successfully parsed.
    pub fn read_count(&self) -> usize {
        self.read
    }
}

impl<R: BufRead, T: DeserializeOwned> Iterator for Reader<R, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        loop {
            self.buf.clear();
            match self.inner.read_line(&mut self.buf) {
                Ok(0) => return None,
                Ok(_) => {}
                Err(_) => return None,
            }
            let line = self.buf.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str(line) {
                Ok(v) => {
                    self.read += 1;
                    return Some(v);
                }
                Err(_) => {
                    self.skipped += 1;
                    continue;
                }
            }
        }
    }
}

/// Write one record and its newline.
///
/// One record per write call, flushed by the caller. NDJSON's whole value is
/// that a reader can make sense of a prefix, which requires that a record and
/// its terminator reach the file together.
pub fn write_one<W: Write, T: Serialize>(w: &mut W, value: &T) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push(b'\n');
    w.write_all(&line)
}

/// Write many records.
pub fn write_all<'a, W: Write, T: Serialize + 'a>(
    w: &mut W,
    values: impl IntoIterator<Item = &'a T>,
) -> std::io::Result<()> {
    for v in values {
        write_one(w, v)?;
    }
    Ok(())
}

/// Serialise records to a `String`.
pub fn to_string<'a, T: Serialize + 'a>(
    values: impl IntoIterator<Item = &'a T>,
) -> std::io::Result<String> {
    let mut out = Vec::new();
    write_all(&mut out, values)?;
    String::from_utf8(out)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Row {
        n: u32,
        s: String,
    }

    fn row(n: u32, s: &str) -> Row {
        Row { n, s: s.to_string() }
    }

    #[test]
    fn round_trips() {
        let rows = vec![row(1, "a"), row(2, "b")];
        let text = to_string(&rows).unwrap();
        assert_eq!(text, "{\"n\":1,\"s\":\"a\"}\n{\"n\":2,\"s\":\"b\"}\n");
        let back: Vec<Row> = read_str(&text).collect();
        assert_eq!(back, rows);
    }

    /// The case the whole module exists for: a file being appended to while it
    /// is read ends in half a record. That must not cost the records before it.
    #[test]
    fn a_torn_final_line_does_not_lose_the_file() {
        let text = "{\"n\":1,\"s\":\"a\"}\n{\"n\":2,\"s\":\"b\"}\n{\"n\":3,\"s\":\"c";
        let back: Vec<Row> = read_str(text).collect();
        assert_eq!(back, vec![row(1, "a"), row(2, "b")]);
    }

    #[test]
    fn blank_lines_are_not_errors_but_bad_lines_are_counted() {
        let text = "{\"n\":1,\"s\":\"a\"}\n\n   \nnot json\n{\"n\":2,\"s\":\"b\"}\n";
        let mut r: Reader<_, Row> = Reader::new(std::io::Cursor::new(text));
        let got: Vec<Row> = r.by_ref().collect();
        assert_eq!(got, vec![row(1, "a"), row(2, "b")]);
        assert_eq!(r.read_count(), 2);
        assert_eq!(r.skipped(), 1, "the blank lines are not failures; `not json` is");
    }

    /// A record whose JSON contains an escaped newline is still one line, and
    /// splitting on a raw newline must not cut it in half.
    #[test]
    fn embedded_newlines_survive() {
        let rows = vec![row(1, "two\nlines")];
        let text = to_string(&rows).unwrap();
        assert_eq!(text.lines().count(), 1);
        let back: Vec<Row> = read_str(&text).collect();
        assert_eq!(back, rows);
    }

    #[test]
    fn empty_input_yields_nothing() {
        let back: Vec<Row> = read_str("").collect();
        assert!(back.is_empty());
        let back: Vec<Row> = read_str("\n\n").collect();
        assert!(back.is_empty());
    }
}
