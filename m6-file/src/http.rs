use anyhow::{bail, Result};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};

/// A parsed HTTP/1.1 request.
#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    #[allow(dead_code)]
    pub query: String,
    /// Headers stored as ordered pairs; linear scan is faster than HashMap for 3-6 entries.
    pub headers: Vec<(String, String)>,
}

impl Request {
    /// Read and parse an HTTP/1.1 request from a stream.
    pub fn read<R: Read>(stream: R) -> Result<Self> {
        let mut reader = BufReader::new(stream);
        let mut request_line = String::new();
        reader.read_line(&mut request_line)?;
        let request_line = request_line.trim_end_matches(|c| c == '\r' || c == '\n');

        let parts: Vec<&str> = request_line.splitn(3, ' ').collect();
        if parts.len() < 3 {
            bail!("invalid request line: {:?}", request_line);
        }
        let method = parts[0].to_string();
        let full_path = parts[1].to_string();
        let (path, query) = if let Some(idx) = full_path.find('?') {
            (full_path[..idx].to_string(), full_path[idx + 1..].to_string())
        } else {
            (full_path, String::new())
        };

        // Read headers into Vec — avoids hashing and is faster for small header counts.
        let mut headers: Vec<(String, String)> = Vec::with_capacity(8);
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let line = line.trim_end_matches(|c| c == '\r' || c == '\n');
            if line.is_empty() {
                break;
            }
            if let Some(idx) = line.find(':') {
                let key = line[..idx].trim().to_lowercase();
                let val = line[idx + 1..].trim().to_string();
                headers.push((key, val));
            }
        }

        Ok(Request { method, path, query, headers })
    }

    /// Return the Accept-Encoding header value, or "" if absent.
    pub fn accept_encoding(&self) -> &str {
        self.headers
            .iter()
            .find(|(k, _)| k == "accept-encoding")
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    }
}

/// Write an HTTP/1.1 response to a stream.
/// Uses BufWriter with direct byte writes — no intermediate String heap allocations.
pub fn write_response<W: Write>(
    stream: &mut W,
    status: u16,
    reason: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<()> {
    let mut w = BufWriter::with_capacity(512, stream);
    write!(w, "HTTP/1.1 {} {}\r\n", status, reason)?;
    for (k, v) in headers {
        w.write_all(k.as_bytes())?;
        w.write_all(b": ")?;
        w.write_all(v.as_bytes())?;
        w.write_all(b"\r\n")?;
    }
    write!(w, "Content-Length: {}\r\nConnection: close\r\n\r\n", body.len())?;
    w.write_all(body)?;
    w.flush()?;
    Ok(())
}

/// Write an HTTP/1.1 HEAD response (headers only, no body).
/// `body_len` is the length of the body that *would* be sent for GET, so that
/// `Content-Length` reflects the correct value per RFC 7231 §3.3.
pub fn write_head_response<W: Write>(
    stream: &mut W,
    status: u16,
    reason: &str,
    headers: &[(&str, &str)],
    body_len: usize,
) -> Result<()> {
    let mut w = BufWriter::with_capacity(512, stream);
    write!(w, "HTTP/1.1 {} {}\r\n", status, reason)?;
    for (k, v) in headers {
        w.write_all(k.as_bytes())?;
        w.write_all(b": ")?;
        w.write_all(v.as_bytes())?;
        w.write_all(b"\r\n")?;
    }
    write!(w, "Content-Length: {}\r\nConnection: close\r\n\r\n", body_len)?;
    w.flush()?;
    Ok(())
}

/// Write a simple error response.
pub fn write_error<W: Write>(stream: &mut W, status: u16, reason: &str) -> Result<()> {
    let body = format!("{} {}", status, reason);
    write_response(
        stream,
        status,
        reason,
        &[("Content-Type", "text/plain")],
        body.as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_parse_request() {
        let raw = b"GET /assets/css/main.css HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: br, gzip\r\n\r\n";
        let req = Request::read(Cursor::new(raw)).unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/assets/css/main.css");
        assert_eq!(req.accept_encoding(), "br, gzip");
    }

    #[test]
    fn test_parse_request_with_query() {
        let raw = b"GET /path?foo=bar HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let req = Request::read(Cursor::new(raw)).unwrap();
        assert_eq!(req.path, "/path");
        assert_eq!(req.query, "foo=bar");
    }
}
