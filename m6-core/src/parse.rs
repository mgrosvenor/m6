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
    // Read headers section byte-by-byte until we see \r\n\r\n.
    let mut header_buf: Vec<u8> = Vec::with_capacity(1024);
    let mut single = [0u8; 1];

    loop {
        match stream.read(&mut single) {
            Ok(0) => {
                if header_buf.is_empty() {
                    return Err(ParseError::ConnectionClosed);
                }
                return Err(ParseError::InvalidRequestLine);
            }
            Ok(_) => {}
            Err(e) => return Err(ParseError::Io(e)),
        }
        header_buf.push(single[0]);
        if header_buf.len() > MAX_HEADER_BYTES {
            return Err(ParseError::RequestTooLarge);
        }
        // Check for \r\n\r\n terminator.
        let n = header_buf.len();
        if n >= 4
            && header_buf[n - 4] == b'\r'
            && header_buf[n - 3] == b'\n'
            && header_buf[n - 2] == b'\r'
            && header_buf[n - 1] == b'\n'
        {
            break;
        }
    }

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

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        stream.read_exact(&mut body).map_err(|e| {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                ParseError::ConnectionClosed
            } else {
                ParseError::Io(e)
            }
        })?;
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
