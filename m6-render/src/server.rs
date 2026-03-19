/// Unix socket HTTP/1.1 server — connection accept loop and HTTP parsing.
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::os::unix::net::UnixStream;

use anyhow::Context;

use crate::request::RawRequest;

/// Maximum request body size (16 MiB default).
const MAX_BODY_SIZE: usize = 16 * 1024 * 1024;

/// Parse an HTTP/1.1 request from a Unix stream.
///
/// Returns `None` if the connection closed without sending anything.
pub fn parse_request(stream: &mut UnixStream) -> anyhow::Result<Option<RawRequest>> {
    let mut reader = BufReader::new(stream as &mut dyn Read);

    // Read the request line.
    let mut request_line = String::new();
    let n = reader.read_line(&mut request_line).context("reading request line")?;
    if n == 0 {
        return Ok(None); // EOF — connection closed
    }

    let request_line = request_line.trim_end_matches(|c: char| c == '\r' || c == '\n');
    let mut parts = request_line.splitn(3, ' ');
    let method = parts.next().unwrap_or("").to_uppercase();
    let full_path = parts.next().unwrap_or("/").to_string();

    // Split path and query string.
    let (path, query) = if let Some(idx) = full_path.find('?') {
        (full_path[..idx].to_string(), full_path[idx + 1..].to_string())
    } else {
        (full_path, String::new())
    };

    // Read headers into Vec — lowercase keys, linear scan beats HashMap for 4-8 entries.
    let mut headers: Vec<(String, String)> = Vec::with_capacity(8);
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).context("reading header")?;
        let line = line.trim_end_matches(|c| c == '\r' || c == '\n');
        if line.is_empty() {
            break; // end of headers
        }
        if let Some(idx) = line.find(':') {
            let name = line[..idx].trim().to_lowercase();
            let value = line[idx + 1..].trim().to_string();
            headers.push((name, value));
        }
    }

    // Read body if Content-Length present.
    let body = if let Some((_, len_str)) = headers.iter().find(|(k, _)| k == "content-length") {
        let len: usize = len_str
            .trim()
            .parse()
            .context("parsing Content-Length")?;
        if len > MAX_BODY_SIZE {
            anyhow::bail!("request body too large: {len}");
        }
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body).context("reading body")?;
        body
    } else {
        vec![]
    };

    Ok(Some(RawRequest { method, path, query, headers, body }))
}

/// Write a complete HTTP response to the stream using BufWriter —
/// avoids building an intermediate Vec<u8> copy of the response.
pub fn write_response(stream: &mut UnixStream, response: &crate::response::Response) -> anyhow::Result<()> {
    let mut w = BufWriter::with_capacity(512, stream as &mut dyn Write);
    response.write_to(&mut w)?;
    w.flush().context("flushing response")?;
    Ok(())
}

/// Write a minimal error response without a full `Response` struct.
pub fn write_error_response(stream: &mut UnixStream, status: u16, body: &str) -> anyhow::Result<()> {
    let resp = crate::response::Response {
        status,
        headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
        body: body.as_bytes().to_vec(),
        template_name: None,
            template_dict: None,
    };
    write_response(stream, &resp)
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
        assert_eq!(req.query, "foo=bar");
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
            "POST /submit HTTP/1.1\r\nContent-Length: {}\r\nContent-Type: application/x-www-form-urlencoded\r\n\r\n",
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
