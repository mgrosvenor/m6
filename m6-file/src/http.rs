use anyhow::Result;
use std::io::{BufWriter, Write};

/// The request type is `m6_core::http::RawRequest`, parsed by the one parser
/// in `m6_core::h1`.
///
/// This file used to carry its own. Measured against h1spec, the independent
/// RFC 9112 tester, it scored 15/32 against the shared parser's 27/32, and it
/// had no limit on the request line, the header count, or the length of any
/// header -- it read with `BufReader::read_line` until a newline arrived, so a
/// peer that never sent one made it allocate without bound.
///
/// It also lowercased header names at parse time, which is why the handler
/// compared them with `k == "if-none-match"`. That is correct only under an
/// invariant established in this file and invisible at the call site; the
/// shared parser keeps names as sent and lookups go through
/// `m6_core::header`, which is case-insensitive and needs no invariant.
pub use m6_core::http::RawRequest as Request;

/// The `Accept-Encoding` value, or `""` when absent.
pub fn accept_encoding(req: &Request) -> &str {
    m6_core::header(&req.headers, "accept-encoding").unwrap_or("")
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
        let req = m6_core::parse::parse_request(&mut Cursor::new(raw)).unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/assets/css/main.css");
        assert_eq!(accept_encoding(&req), "br, gzip");
    }

    #[test]
    fn test_parse_request_with_query() {
        let raw = b"GET /path?foo=bar HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let req = m6_core::parse::parse_request(&mut Cursor::new(raw)).unwrap();
        assert_eq!(req.path, "/path");
        assert_eq!(req.query.as_deref(), Some("foo=bar"));
    }
}
