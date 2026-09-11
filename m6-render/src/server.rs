/// Unix socket HTTP/1.1 server — connection accept loop and HTTP parsing.
use std::os::unix::net::UnixStream;


use crate::request::RawRequest;

/// Maximum request body size (16 MiB default).
// The body cap lives with the parser now, in m6_core::parse.

/// Parse an HTTP/1.1 request from a Unix stream.
///
/// Delegates to the one parser, `m6_core::h1`, via its streaming wrapper.
/// This function used to contain a second implementation: its own request-line
/// and header reader over `BufReader::read_line`, scoring 15/32 on h1spec
/// against the shared parser's 27/32, with no cap on the request line or on
/// the number of headers.
///
/// Returns `None` if the connection closed without sending anything, which is
/// an idle keep-alive connection going away rather than an error.
pub fn parse_request(stream: &mut UnixStream) -> anyhow::Result<Option<RawRequest>> {
    match m6_core::parse::parse_request(stream) {
        Ok(req) => Ok(Some(req)),
        Err(m6_core::parse::ParseError::ConnectionClosed) => Ok(None),
        Err(e) => Err(anyhow::anyhow!(e)),
    }
}

/// Send a response through the one HTTP/1.1 response writer.
pub fn write_response<W: std::io::Write>(
    resp: &mut m6_core::h1::Responder<'_, W>,
    response: &crate::response::Response,
) -> anyhow::Result<()> {
    response.send(resp)
}

/// A minimal error response, for the paths that have no `Response` to send:
/// a full thread pool, or a request that never parsed.
pub fn write_error_response<W: std::io::Write>(
    stream: &mut W,
    status: u16,
    _body: &str,
) -> anyhow::Result<()> {
    // Not a persistent connection: these are the paths where the server is
    // giving up on the connection, not serving it.
    let mut resp = m6_core::h1::Responder::new(stream, "", false);
    resp.error(status)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixListener;
    use tempfile::tempdir;

    #[test]
    fn test_parse_get_request() {
        let dir = tempdir().unwrap();
        let sock_path = dir.path().join("test.sock");

        let listener = UnixListener::bind(&sock_path).unwrap();
        let sock_path2 = sock_path.clone();

        let handle = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            parse_request(&mut conn).unwrap()
        });

        let mut client = UnixStream::connect(&sock_path2).unwrap();
        client.write_all(b"GET /test?foo=bar HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        drop(client);

        let req = handle.join().unwrap().unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/test");
        assert_eq!(req.query.as_deref(), Some("foo=bar"));
        assert_eq!(req.header("host").unwrap(), "localhost");
    }

    #[test]
    fn test_parse_post_request() {
        let dir = tempdir().unwrap();
        let sock_path = dir.path().join("test2.sock");

        let listener = UnixListener::bind(&sock_path).unwrap();
        let sock_path2 = sock_path.clone();

        let handle = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            parse_request(&mut conn).unwrap()
        });

        let body = b"name=alice&age=30";
        let request = format!(
            // Host is required on HTTP/1.1 (RFC 9110 7.2). The parser this
            // fixture predates accepted a request without one; the shared
            // parser does not, and that is h1spec test 8.
            "POST /submit HTTP/1.1\r\nHost: localhost\r\n\
             Content-Length: {}\r\nContent-Type: application/x-www-form-urlencoded\r\n\r\n",
            body.len()
        );
        let mut client = UnixStream::connect(&sock_path2).unwrap();
        client.write_all(request.as_bytes()).unwrap();
        client.write_all(body).unwrap();
        drop(client);

        let req = handle.join().unwrap().unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/submit");
        assert_eq!(req.body, body);
    }
}
