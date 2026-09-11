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
    // Read in chunks until the one parser in `crate::h1` says the message is
    // complete. It decides completeness, including the body, so this loop has
    // no framing logic of its own -- which is the point: there is one place
    // where HTTP/1.1 framing is decided.
    //
    // This used to read **one byte per `read()` call**: 377 syscalls for a
    // 377-byte request head from an ordinary browser, measured, and nothing
    // buffered it because `crate::server` hands the raw `UnixStream` straight
    // in.
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 1024];

    loop {
        match crate::h1::parse_request(&buf) {
            crate::h1::ParseResult::Complete(req) => return Ok(req),
            crate::h1::ParseResult::Error => return Err(ParseError::InvalidHeader),
            crate::h1::ParseResult::Incomplete => {}
        }

        // Two caps, because they guard different things. Until the blank line
        // arrives everything read is head, and an unbounded head is how a peer
        // makes the server buffer forever. After it, the body is bounded
        // separately and much more generously.
        let head_done = buf.windows(4).any(|w| w == b"\r\n\r\n");
        let cap = if head_done { MAX_HEADER_BYTES + MAX_BODY_BYTES } else { MAX_HEADER_BYTES };
        if buf.len() > cap {
            return Err(ParseError::RequestTooLarge);
        }

        match stream.read(&mut chunk) {
            Ok(0) => {
                if buf.is_empty() {
                    return Err(ParseError::ConnectionClosed);
                }
                // The peer half-closed with an incomplete message. Nothing more
                // is coming, so it cannot become complete.
                return Err(ParseError::InvalidRequestLine);
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) => return Err(ParseError::Io(e)),
        }
    }
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
            "POST /submit HTTP/1.1\r\nHost: localhost\r\n\
             Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n",
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
        // Either refusal is correct. The parser holds headers in a fixed
        // 64-entry stack array -- no allocation, which is the point -- so a
        // flood is refused as a malformed head before the byte cap is reached.
        // What matters is that it is refused, not which reason wins the race.
        assert!(parse_request(&mut r).is_err(), "an oversized head must be refused");
    }
}

#[cfg(test)]
mod stricter_after_consolidation_tests {
    use super::*;
    use std::io::Cursor;

    /// RFC 9110 7.2: an HTTP/1.1 request without `Host` is malformed.
    ///
    /// The parser this replaced accepted it, which is h1spec test 8 and one of
    /// the reasons that implementation scored 14/32. The fixture for
    /// `test_parse_post_with_body` had no `Host` and passed for the same
    /// reason; it does now because the request it builds is valid, not because
    /// the parser stopped checking.
    #[test]
    fn http11_without_host_is_refused() {
        let raw = b"GET /x HTTP/1.1\r\nAccept: */*\r\n\r\n";
        let mut c = Cursor::new(raw.to_vec());
        assert!(parse_request(&mut c).is_err(), "HTTP/1.1 without Host must be refused");
    }

    /// Two `Host` headers are malformed however they disagree: it is the
    /// ambiguity that routes a request two ways in two hops.
    #[test]
    fn duplicate_host_is_refused() {
        let raw = b"GET /x HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n";
        let mut c = Cursor::new(raw.to_vec());
        assert!(parse_request(&mut c).is_err(), "duplicate Host must be refused");
    }

    /// Conflicting Content-Length is the request-smuggling primitive.
    #[test]
    fn conflicting_content_length_is_refused() {
        let mut raw = b"POST /x HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\n".to_vec();
        raw.extend_from_slice(b"hello");
        let mut c = Cursor::new(raw);
        assert!(parse_request(&mut c).is_err(), "conflicting Content-Length must be refused");
    }

    /// A head with no end is how a peer makes the server buffer forever.
    ///
    /// Refused either as too large or as malformed: the 64-entry stack array
    /// the parser uses for headers fills before the byte cap does. Both are
    /// refusals and the test asserts the property, not the race.
    #[test]
    fn an_endless_head_is_capped() {
        let mut raw = b"GET / HTTP/1.1\r\nHost: a\r\n".to_vec();
        for i in 0..600 {
            raw.extend_from_slice(format!("X-Pad-{i}: {}\r\n", "a".repeat(64)).as_bytes());
        }
        // Deliberately never terminated.
        let mut c = Cursor::new(raw);
        assert!(
            parse_request(&mut c).is_err(),
            "an unterminated oversized head must be refused"
        );
    }
}
