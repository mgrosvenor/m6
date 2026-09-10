/// HTTP/1.1 request parser from a byte stream.

use std::io::Read;

use crate::http::RawRequest;

/// Maximum size for request line + headers combined (8 KB).
const MAX_HEADER_BYTES: usize = 8 * 1024;
/// Maximum body size (16 MB).
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("connection closed")]
    ConnectionClosed,
    #[error("invalid request line")]
    InvalidRequestLine,
    #[error("invalid header")]
    InvalidHeader,
    #[error("request too large")]
    RequestTooLarge,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Parse a complete HTTP/1.1 request from a Read stream.
///
/// Handles: request line, headers, body (Content-Length based).
/// Does not handle chunked transfer encoding.
pub fn parse_request(stream: &mut impl Read) -> Result<RawRequest, ParseError> {
    // Read the head in chunks until `\r\n\r\n`.
    //
    // This used to read **one byte per `read()` call**: 377 syscalls for a
    // 377-byte request head from an ordinary browser, measured. Nothing
    // buffered it either -- `m6_core::server::handle_connection` hands the raw
    // `UnixStream` straight in -- so every one of those was a real syscall.
    //
    // A chunked read can overshoot the terminator and take the first bytes of
    // the body with it, which is why the byte-at-a-time version existed. The
    // overshoot is kept in `body` below rather than discarded, so nothing is
    // lost and the body read starts from what was already pulled in.
    let mut header_buf: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 1024];
    let mut head_len: Option<usize> = None;

    while head_len.is_none() {
        // Rescan only from just before the tail already examined, so a
        // terminator straddling two chunks is still found without rescanning
        // the whole buffer each time.
        let scan_from = header_buf.len().saturating_sub(3);
        let n = match stream.read(&mut chunk) {
            Ok(0) => {
                if header_buf.is_empty() {
                    return Err(ParseError::ConnectionClosed);
                }
                return Err(ParseError::InvalidRequestLine);
            }
            Ok(n) => n,
            Err(e) => return Err(ParseError::Io(e)),
        };
        header_buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = header_buf[scan_from..]
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
        {
            head_len = Some(scan_from + pos + 4);
        }
        // The cap applies to the head, so check it against what the head could
        // still be, not against the overshoot.
        if head_len.is_none() && header_buf.len() > MAX_HEADER_BYTES {
            return Err(ParseError::RequestTooLarge);
        }
    }
    let head_len = head_len.expect("loop exits only when set");
    if head_len > MAX_HEADER_BYTES {
        return Err(ParseError::RequestTooLarge);
    }
    // Anything past the terminator is the first of the body.
    let body_prefix = header_buf.split_off(head_len);

    // Split into lines.
    let header_str = std::str::from_utf8(&header_buf)
        .map_err(|_| ParseError::InvalidRequestLine)?;

    let mut lines = header_str.split("\r\n");

    // Parse request line: METHOD path?query HTTP/1.1
    let request_line = lines.next().ok_or(ParseError::InvalidRequestLine)?;
    let mut parts = request_line.splitn(3, ' ');

    let method = parts
        .next()
        .ok_or(ParseError::InvalidRequestLine)?
        .to_string();
    let raw_target = parts
        .next()
        .ok_or(ParseError::InvalidRequestLine)?;
    let _version = parts
        .next()
        .ok_or(ParseError::InvalidRequestLine)?;

    if method.is_empty() || raw_target.is_empty() {
        return Err(ParseError::InvalidRequestLine);
    }

    // Split path from query.
    let (path, query) = if let Some(pos) = raw_target.find('?') {
        let q = raw_target[pos + 1..].to_string();
        (raw_target[..pos].to_string(), Some(q))
    } else {
        (raw_target.to_string(), None)
    };

    // Parse headers.
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        // Empty line marks end of headers (the \r\n\r\n split produces an empty entry).
        if line.is_empty() {
            break;
        }
        let colon = line.find(':').ok_or(ParseError::InvalidHeader)?;
        let name = line[..colon].trim().to_string();
        let value = line[colon + 1..].trim().to_string();
        if name.is_empty() {
            return Err(ParseError::InvalidHeader);
        }
        headers.push((name, value));
    }

    // Determine body length from Content-Length header.
    let content_length: usize = headers
        .iter()
        .find(|(k, _)| k.to_ascii_lowercase() == "content-length")
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0);

    if content_length > MAX_BODY_BYTES {
        return Err(ParseError::RequestTooLarge);
    }

    // Start from whatever the chunked head read already pulled past the
    // terminator, then read only the remainder. Discarding the overshoot would
    // silently truncate every body that arrived in the same packet as its
    // headers, which is the common case.
    let mut body = body_prefix;
    if body.len() > content_length {
        body.truncate(content_length);
    }
    if body.len() < content_length {
        let mut rest = vec![0u8; content_length - body.len()];
        stream.read_exact(&mut rest).map_err(|e| {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                ParseError::ConnectionClosed
            } else {
                ParseError::Io(e)
            }
        })?;
        body.extend_from_slice(&rest);
    }

    Ok(RawRequest {
        method,
        path,
        query,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_parse_get_request() {
        let raw = b"GET /index.html HTTP/1.1\r\nHost: localhost\r\nAccept: text/html\r\n\r\n";
        let mut cursor = Cursor::new(raw);
        let req = parse_request(&mut cursor).unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/index.html");
        assert_eq!(req.query, None);
        assert_eq!(req.header("host"), Some("localhost"));
        assert!(req.body.is_empty());
    }

    #[test]
    fn test_parse_get_with_query() {
        let raw = b"GET /search?q=foo&page=2 HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let mut cursor = Cursor::new(raw);
        let req = parse_request(&mut cursor).unwrap();
        assert_eq!(req.path, "/search");
        assert_eq!(req.query.as_deref(), Some("q=foo&page=2"));
    }

    #[test]
    fn test_parse_post_with_body() {
        let body = b"name=alice&age=30";
        let raw = format!(
            "POST /submit HTTP/1.1\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut bytes = raw.into_bytes();
        bytes.extend_from_slice(body);
        let mut cursor = Cursor::new(bytes);
        let req = parse_request(&mut cursor).unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.body, body);
    }

    #[test]
    fn test_parse_empty_stream_returns_closed() {
        let raw: &[u8] = b"";
        let mut cursor = Cursor::new(raw);
        let err = parse_request(&mut cursor).unwrap_err();
        assert!(matches!(err, ParseError::ConnectionClosed));
    }

    #[test]
    fn test_parse_missing_content_length_defaults_to_no_body() {
        // POST without Content-Length — body should be empty.
        let raw = b"POST /submit HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let mut cursor = Cursor::new(raw);
        let req = parse_request(&mut cursor).unwrap();
        assert!(req.body.is_empty());
    }
}

#[cfg(test)]
mod chunked_head_tests {
    use super::*;
    use std::io::Read;

    /// A reader that counts `read` calls and can be told to hand over the
    /// bytes in fixed-size pieces, so a terminator can be forced to straddle
    /// a chunk boundary.
    struct Chunked {
        data: Vec<u8>,
        at: usize,
        piece: usize,
        pub reads: usize,
    }
    impl Chunked {
        fn new(data: &[u8], piece: usize) -> Self {
            Chunked { data: data.to_vec(), at: 0, piece, reads: 0 }
        }
    }
    impl Read for Chunked {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reads += 1;
            let n = self.piece.min(buf.len()).min(self.data.len() - self.at);
            buf[..n].copy_from_slice(&self.data[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
    }

    fn browser_request() -> Vec<u8> {
        b"GET /capabilities HTTP/1.1\r\n\
Host: mgrosvenor.com\r\n\
User-Agent: Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36\r\n\
Accept: text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8\r\n\
Accept-Encoding: gzip, deflate, br, zstd\r\n\
Connection: keep-alive\r\n\r\n"
            .to_vec()
    }

    /// The head is read in chunks, not one byte per syscall.
    ///
    /// It used to be one `read` per byte: 377 calls for a 377-byte head from
    /// an ordinary browser, and nothing buffered it, so every one was a real
    /// syscall. `m6_core::server` hands the raw `UnixStream` straight in.
    #[test]
    fn the_head_is_not_read_one_byte_at_a_time() {
        let req = browser_request();
        let mut r = Chunked::new(&req, 1024);
        let parsed = parse_request(&mut r).expect("parse");
        assert_eq!(parsed.headers.len(), 5);
        assert!(
            r.reads <= 4,
            "{} read() calls for a {}-byte head; the byte-at-a-time version \
             took one per byte",
            r.reads,
            req.len()
        );
    }

    /// A chunked read can land the `\r\n\r\n` across a boundary. Finding it
    /// requires rescanning the tail of what was already examined.
    #[test]
    fn a_terminator_split_across_chunks_is_still_found() {
        let req = browser_request();
        for piece in [1, 2, 3, 5, 7, 13, 64, 377] {
            let mut r = Chunked::new(&req, piece);
            let parsed = parse_request(&mut r)
                .unwrap_or_else(|e| panic!("piece={piece}: {e}"));
            assert_eq!(parsed.method, "GET", "piece={piece}");
            assert_eq!(parsed.headers.len(), 5, "piece={piece}");
        }
    }

    /// The head read overshoots into the body whenever both arrive together,
    /// which is the common case. Discarding the overshoot would truncate the
    /// body silently.
    #[test]
    fn a_body_arriving_with_its_headers_is_not_truncated() {
        let mut req = b"POST /submit HTTP/1.1\r\nHost: x\r\nContent-Length: 17\r\n\r\n".to_vec();
        req.extend_from_slice(b"name=alice&age=30");
        for piece in [1, 8, 64, 4096] {
            let mut r = Chunked::new(&req, piece);
            let parsed = parse_request(&mut r)
                .unwrap_or_else(|e| panic!("piece={piece}: {e}"));
            assert_eq!(
                parsed.body, b"name=alice&age=30",
                "body truncated or corrupted at piece={piece}"
            );
        }
    }

    /// More bytes after the body than Content-Length claims must not leak into
    /// it: a pipelined second request follows on the same connection.
    #[test]
    fn overshoot_past_content_length_does_not_join_the_body() {
        let mut req = b"POST /a HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\n".to_vec();
        req.extend_from_slice(b"HELLOGET /b HTTP/1.1\r\nHost: x\r\n\r\n");
        let mut r = Chunked::new(&req, 4096);
        let parsed = parse_request(&mut r).expect("parse");
        assert_eq!(parsed.body, b"HELLO", "the next request bled into the body");
    }

    /// The size cap still applies to the head.
    #[test]
    fn an_oversized_head_is_still_refused() {
        let mut req = b"GET / HTTP/1.1\r\n".to_vec();
        for i in 0..600 {
            req.extend_from_slice(format!("X-Pad-{i}: {}\r\n", "a".repeat(64)).as_bytes());
        }
        req.extend_from_slice(b"\r\n");
        let mut r = Chunked::new(&req, 4096);
        assert!(
            matches!(parse_request(&mut r), Err(ParseError::RequestTooLarge)),
            "an oversized head must be refused"
        );
    }
}
