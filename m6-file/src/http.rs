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

/// Responses are written by `m6_core::h1::Responder`, handed to the handler
/// by `m6_core::server::serve_connection`.
///
/// This file used to carry three writers of its own -- `write_response`,
/// `write_head_response` and `write_error` -- all hardcoding
/// `Connection: close`, and only one of the three omitting the body on a HEAD.
/// So every 404, 405, 400 and 412 answering a HEAD went out with a body, and
/// no connection was ever reused. m6-html and m6-auth-server each had their
/// own near-copies with their own versions of the same two defects.
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_parse_request() {
        let raw = b"GET /assets/css/main.css HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: br, gzip\r\n\r\n";
        let req = m6_core::parse::parse_request(&mut Cursor::new(raw.to_vec())).unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/assets/css/main.css");
        assert_eq!(accept_encoding(&req), "br, gzip");
    }

    #[test]
    fn test_parse_request_with_query() {
        let raw = b"GET /path?foo=bar HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let req = m6_core::parse::parse_request(&mut Cursor::new(raw.to_vec())).unwrap();
        assert_eq!(req.path, "/path");
        assert_eq!(req.query.as_deref(), Some("foo=bar"));
    }
}
