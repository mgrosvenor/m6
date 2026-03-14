/// HTTP/1.1 request forwarding over Unix sockets (synchronous blocking I/O).
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

/// Hop-by-hop headers to remove before forwarding.
pub const HOP_BY_HOP: &[&str] = &[
    "connection",
    "upgrade",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "proxy-authorization",
    "proxy-connection",
];

/// A parsed HTTP request (simplified for forwarding).
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    pub version: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// A parsed HTTP response.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Write a usize as decimal into a stack buffer; return the filled slice.
#[inline(always)]
fn write_decimal(mut n: usize, buf: &mut [u8; 20]) -> &[u8] {
    let mut end = buf.len();
    if n == 0 {
        buf[end - 1] = b'0';
        return &buf[end - 1..];
    }
    while n > 0 {
        end -= 1;
        buf[end] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    &buf[end..]
}

/// Forward an HTTP request over a Unix socket (blocking).
/// Adds proxy headers, removes hop-by-hop headers.
/// `timeout` sets both the read and write timeout; `None` means no timeout.
pub fn forward_request(
    socket_path: &Path,
    req: &HttpRequest,
    client_ip: &str,
    original_host: &str,
) -> io::Result<HttpResponse> {
    forward_request_timeout(socket_path, req, client_ip, original_host, None)
}

/// Like `forward_request` but with a configurable timeout for the total
/// backend call (connect + write + read). A `TimedOut` error is returned if
/// the backend does not respond within the deadline.
pub fn forward_request_timeout(
    socket_path: &Path,
    req: &HttpRequest,
    client_ip: &str,
    original_host: &str,
    timeout: Option<std::time::Duration>,
) -> io::Result<HttpResponse> {
    let mut stream = UnixStream::connect(socket_path)?;

    if let Some(dur) = timeout {
        stream.set_read_timeout(Some(dur))?;
        stream.set_write_timeout(Some(dur))?;
    }

    // Build request bytes
    let mut buf = Vec::with_capacity(8192);

    // Write request line
    buf.extend_from_slice(req.method.as_bytes());
    buf.push(b' ');
    buf.extend_from_slice(req.path.as_bytes());
    if let Some(ref q) = req.query {
        if !q.is_empty() {
            buf.push(b'?');
            buf.extend_from_slice(q.as_bytes());
        }
    }
    buf.extend_from_slice(b" HTTP/1.1\r\n");

    // Forward headers, excluding hop-by-hop
    for (name, value) in &req.headers {
        if HOP_BY_HOP.iter().any(|&h| name.eq_ignore_ascii_case(h)) {
            continue;
        }
        buf.extend_from_slice(name.as_bytes());
        buf.extend_from_slice(b": ");
        buf.extend_from_slice(value.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }

    // Add proxy headers
    buf.extend_from_slice(b"X-Forwarded-For: ");
    buf.extend_from_slice(client_ip.as_bytes());
    buf.extend_from_slice(b"\r\nX-Forwarded-Proto: https\r\nX-Forwarded-Host: ");
    buf.extend_from_slice(original_host.as_bytes());
    buf.extend_from_slice(b"\r\n");

    // Content-Length for body — write decimal without allocating.
    if !req.body.is_empty() {
        buf.extend_from_slice(b"Content-Length: ");
        let mut cl_buf = [0u8; 20];
        buf.extend_from_slice(write_decimal(req.body.len(), &mut cl_buf));
        buf.extend_from_slice(b"\r\n");
    }

    // Connection: close for HTTP/1.1 since we're doing per-request connections
    buf.extend_from_slice(b"Connection: close\r\n\r\n");

    // Write body
    if !req.body.is_empty() {
        buf.extend_from_slice(&req.body);
    }

    stream.write_all(&buf)?;
    stream.flush()?;

    // Read response
    read_response(stream)
}

/// Read an HTTP/1.1 response from a synchronous stream.
pub fn read_response<R: Read>(mut reader: R) -> io::Result<HttpResponse> {
    // 8 KiB on the stack — sufficient for all normal responses, no heap alloc.
    let mut header_buf = [0u8; 8192];
    let mut n_total = 0usize;
    let header_end;

    loop {
        let n = reader.read(&mut header_buf[n_total..])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before headers complete",
            ));
        }
        n_total += n;
        if let Some(pos) = find_header_end(&header_buf[..n_total]) {
            header_end = pos;
            break;
        }
        if n_total >= header_buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "response headers too large",
            ));
        }
    }

    let header_section = std::str::from_utf8(&header_buf[..header_end])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "response headers not UTF-8"))?;

    let mut lines = header_section.split("\r\n");

    // Status line
    let status_line = lines.next().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "empty response")
    })?;
    let (status, reason) = parse_status_line(status_line)?;

    let mut headers: Vec<(String, String)> = Vec::with_capacity(16);
    let mut content_length: Option<usize> = None;
    let mut chunked = false;

    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some(colon) = line.find(':') {
            let name = line[..colon].trim();
            let value = line[colon + 1..].trim();
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.parse().ok();
            }
            if name.eq_ignore_ascii_case("transfer-encoding")
                && value.eq_ignore_ascii_case("chunked")
            {
                chunked = true;
            }
            headers.push((name.to_owned(), value.to_owned()));
        }
    }

    // Body bytes that arrived in the same read as the headers — borrow from stack buffer.
    let body_prefix = &header_buf[header_end + 4..n_total];

    let body = if chunked {
        read_chunked_body(&mut reader, body_prefix.to_vec())?
    } else if let Some(len) = content_length {
        let mut body = vec![0u8; len];
        let already = body_prefix.len().min(len);
        body[..already].copy_from_slice(&body_prefix[..already]);
        if already < len {
            reader.read_exact(&mut body[already..])?;
        }
        body
    } else {
        let mut body = body_prefix.to_vec();
        reader.read_to_end(&mut body)?;
        body
    };

    Ok(HttpResponse { status, reason, body, headers })
}

/// Locate the end of the HTTP header section (\r\n\r\n).
/// Returns the byte offset of the first `\r` in the terminating `\r\n\r\n`.
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Parse the status line `HTTP/x.y NNN Reason` into (status_code, reason).
fn parse_status_line(line: &str) -> io::Result<(u16, String)> {
    let mut parts = line.splitn(3, ' ');
    // Skip the version token
    parts.next();
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad or missing status code"))?;
    let reason = parts.next().unwrap_or("").to_owned();
    Ok((status, reason))
}

fn read_chunked_body<R: Read>(reader: &mut R, prefix: Vec<u8>) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();

    // Buffer that holds already-read but not-yet-consumed bytes
    let mut pending: Vec<u8> = prefix;

    // Read all remaining bytes first (chunked bodies are typically small for backend responses)
    let mut rest = Vec::new();
    reader.read_to_end(&mut rest)?;
    pending.extend_from_slice(&rest);

    // Now parse chunks from `pending`
    let mut pos = 0usize;

    loop {
        // Find CRLF for chunk size line
        let crlf = find_crlf(&pending[pos..]).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "chunked: missing CRLF after size")
        })?;
        let size_line = std::str::from_utf8(&pending[pos..pos + crlf])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "chunked: size not utf8"))?;
        let size_str = size_line.trim().split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid chunk size"))?;
        pos += crlf + 2; // skip CRLF

        if size == 0 {
            break;
        }

        if pos + size > pending.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "chunked: data shorter than declared size",
            ));
        }

        body.extend_from_slice(&pending[pos..pos + size]);
        pos += size;

        // Skip trailing CRLF after chunk data
        if pos + 2 <= pending.len() && &pending[pos..pos + 2] == b"\r\n" {
            pos += 2;
        }
    }

    Ok(body)
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

/// Parse an incoming HTTP/1.1 request from a byte buffer.
pub fn parse_request(data: &[u8]) -> Result<HttpRequest, String> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);

    let body_start = match req.parse(data) {
        Ok(httparse::Status::Complete(n)) => n,
        Ok(httparse::Status::Partial) => return Err("incomplete request".to_string()),
        Err(e) => return Err(format!("parse error: {}", e)),
    };

    let method = req.method.ok_or("missing method")?.to_string();
    let raw_path = req.path.ok_or("missing path")?.to_string();
    let version = match req.version {
        Some(1) => "HTTP/1.1".to_string(),
        Some(0) => "HTTP/1.0".to_string(),
        _ => "HTTP/1.1".to_string(),
    };

    let (path, query) = if let Some(idx) = raw_path.find('?') {
        let p = raw_path[..idx].to_string();
        let q = raw_path[idx + 1..].to_string();
        (p, Some(q))
    } else {
        (raw_path, None)
    };

    let mut parsed_headers = Vec::new();
    for h in req.headers.iter() {
        if h.name.is_empty() {
            break;
        }
        let value = std::str::from_utf8(h.value)
            .map_err(|_| "header value not UTF-8")?
            .to_string();
        parsed_headers.push((h.name.to_string(), value));
    }

    let body = data[body_start..].to_vec();

    Ok(HttpRequest { method, path, query, version, headers: parsed_headers, body })
}

/// Forward a request to a URL backend over HTTP/1.1 + TLS.
///
/// `base_url` must be `https://host[:port]` or `http://host[:port]`.
/// ALPN is negotiated (h2 / http/1.1) but only HTTP/1.1 is spoken.
///
/// URL backends are not pooled — a new TCP connection is opened for every
/// request. The `timeout` (if given) is applied as both read and write
/// timeout on the underlying TCP stream.
pub fn forward_url_request(
    base_url: &str,
    req: &HttpRequest,
    client_ip: &str,
    original_host: &str,
    timeout: Option<std::time::Duration>,
) -> io::Result<HttpResponse> {
    // ── Parse URL ────────────────────────────────────────────────────────────
    let (scheme, authority) = parse_url_scheme_authority(base_url)?;
    let use_tls = scheme == "https";
    let (host, port) = split_host_port(&authority, if use_tls { 443 } else { 80 })?;

    // ── TCP connect ──────────────────────────────────────────────────────────
    use std::net::TcpStream;
    let addr = format!("{}:{}", host, port);
    let tcp = TcpStream::connect(&addr)?;
    if let Some(dur) = timeout {
        tcp.set_read_timeout(Some(dur))?;
        tcp.set_write_timeout(Some(dur))?;
    }

    // ── Build request bytes (shared between TLS and plain) ───────────────────
    let req_bytes = build_forwarded_request_bytes(req, &host, client_ip, original_host);

    // ── Send + receive ───────────────────────────────────────────────────────
    if use_tls {
        forward_over_tls(tcp, &host, req_bytes)
    } else {
        use std::io::Write;
        let mut stream = tcp;
        stream.write_all(&req_bytes)?;
        stream.flush()?;
        read_response(stream)
    }
}

/// Build a forwarded HTTP/1.1 request byte buffer (no TLS framing).
fn build_forwarded_request_bytes(
    req: &HttpRequest,
    host: &str,
    client_ip: &str,
    original_host: &str,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4096);

    buf.extend_from_slice(req.method.as_bytes());
    buf.push(b' ');
    buf.extend_from_slice(req.path.as_bytes());
    if let Some(ref q) = req.query {
        if !q.is_empty() {
            buf.push(b'?');
            buf.extend_from_slice(q.as_bytes());
        }
    }
    buf.extend_from_slice(b" HTTP/1.1\r\n");

    // Host header
    buf.extend_from_slice(b"Host: ");
    buf.extend_from_slice(host.as_bytes());
    buf.extend_from_slice(b"\r\n");

    // Forward original headers (minus hop-by-hop and existing Host)
    for (name, value) in &req.headers {
        if HOP_BY_HOP.iter().any(|&h| name.eq_ignore_ascii_case(h)) {
            continue;
        }
        if name.eq_ignore_ascii_case("host") {
            continue; // already written above
        }
        buf.extend_from_slice(name.as_bytes());
        buf.extend_from_slice(b": ");
        buf.extend_from_slice(value.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }

    // Proxy headers
    buf.extend_from_slice(b"X-Forwarded-For: ");
    buf.extend_from_slice(client_ip.as_bytes());
    buf.extend_from_slice(b"\r\nX-Forwarded-Proto: https\r\nX-Forwarded-Host: ");
    buf.extend_from_slice(original_host.as_bytes());
    buf.extend_from_slice(b"\r\n");

    // Content-Length
    if !req.body.is_empty() {
        buf.extend_from_slice(b"Content-Length: ");
        let mut cl_buf = [0u8; 20];
        buf.extend_from_slice(write_decimal(req.body.len(), &mut cl_buf));
        buf.extend_from_slice(b"\r\n");
    }

    buf.extend_from_slice(b"Connection: close\r\n\r\n");

    if !req.body.is_empty() {
        buf.extend_from_slice(&req.body);
    }

    buf
}

/// Parse `scheme://authority` from a URL string.
fn parse_url_scheme_authority(url: &str) -> io::Result<(String, String)> {
    let sep = "://";
    let idx = url.find(sep).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("invalid URL: {}", url))
    })?;
    let scheme = url[..idx].to_lowercase();
    let rest = &url[idx + sep.len()..];
    // Authority ends at the first `/`, `?`, or `#`
    let auth_end = rest
        .find(|c| c == '/' || c == '?' || c == '#')
        .unwrap_or(rest.len());
    let authority = rest[..auth_end].to_string();
    Ok((scheme, authority))
}

/// Split `host:port` or `[ipv6]:port` into (host, port).
fn split_host_port(authority: &str, default_port: u16) -> io::Result<(String, u16)> {
    // IPv6: `[::1]:8443`
    if let Some(bracket_end) = authority.find(']') {
        let host = authority[1..bracket_end].to_string();
        let port = if bracket_end + 1 < authority.len() && authority.as_bytes()[bracket_end + 1] == b':' {
            authority[bracket_end + 2..].parse::<u16>().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid port in URL")
            })?
        } else {
            default_port
        };
        return Ok((host, port));
    }
    if let Some(colon) = authority.rfind(':') {
        if let Ok(p) = authority[colon + 1..].parse::<u16>() {
            return Ok((authority[..colon].to_string(), p));
        }
    }
    Ok((authority.to_string(), default_port))
}

/// Send `req_bytes` over a TLS-wrapped TCP stream to `host` and read back the response.
fn forward_over_tls(
    tcp: std::net::TcpStream,
    host: &str,
    req_bytes: Vec<u8>,
) -> io::Result<HttpResponse> {
    use std::io::Write;
    use std::sync::Arc;
    use rustls::ClientConnection;
    use rustls::StreamOwned;

    // Build TLS config with ALPN h2 + http/1.1.
    let mut root_store = rustls::RootCertStore::empty();
    // Load native system roots first.
    let native_roots = rustls_native_certs::load_native_certs();
    for cert in native_roots.certs {
        let _ = root_store.add(cert);
    }
    // Also include webpki roots as fallback.
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let mut tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    // ALPN: prefer h2 then http/1.1 (we only speak http/1.1 but negotiate h2 to be compatible)
    tls_config.alpn_protocols = vec![b"http/1.1".to_vec(), b"h2".to_vec()];

    let tls_config = Arc::new(tls_config);

    let server_name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("invalid server name: {}", host))
    })?;

    let conn = ClientConnection::new(tls_config, server_name).map_err(|e| {
        io::Error::new(io::ErrorKind::Other, format!("TLS init error: {}", e))
    })?;

    let mut tls_stream = StreamOwned::new(conn, tcp);
    tls_stream.write_all(&req_bytes)?;
    tls_stream.flush()?;
    read_response(tls_stream)
}

/// Build a simple HTTP response buffer (kept for tests).
pub fn build_response(status: u16, reason: &str, headers: &[(String, String)], body: &[u8]) -> Vec<u8> {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = write!(out, "HTTP/1.1 {} {}\r\n", status, reason);
    for (k, v) in headers {
        let _ = write!(out, "{}: {}\r\n", k, v);
    }
    let _ = write!(out, "Content-Length: {}\r\n", body.len());
    let _ = write!(out, "Connection: close\r\n");
    let _ = write!(out, "\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_status_line() {
        let (status, reason) = parse_status_line("HTTP/1.1 200 OK").unwrap();
        assert_eq!(status, 200);
        assert_eq!(reason, "OK");
    }

    #[test]
    fn test_parse_status_line_no_reason() {
        let (status, reason) = parse_status_line("HTTP/1.1 404").unwrap();
        assert_eq!(status, 404);
        assert_eq!(reason, "");
    }

    #[test]
    fn test_read_response_basic() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let resp = read_response(&raw[..]).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"hello");
    }

    #[test]
    fn test_read_response_chunked() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let resp = read_response(&raw[..]).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"hello");
    }

    #[test]
    fn test_parse_request_basic() {
        let raw = b"GET /hello?foo=bar HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let req = parse_request(raw).unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/hello");
        assert_eq!(req.query, Some("foo=bar".to_string()));
    }

    #[test]
    fn test_hop_by_hop_not_forwarded() {
        // The HOP_BY_HOP list should contain expected headers
        assert!(HOP_BY_HOP.contains(&"connection"));
        assert!(HOP_BY_HOP.contains(&"upgrade"));
        assert!(HOP_BY_HOP.contains(&"transfer-encoding"));
        assert!(HOP_BY_HOP.contains(&"keep-alive"));
    }

    #[test]
    fn test_build_response() {
        let buf = build_response(200, "OK", &[], b"hello");
        let s = std::str::from_utf8(&buf).unwrap();
        assert!(s.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(s.contains("Content-Length: 5\r\n"));
        assert!(s.ends_with("hello"));
    }

    // ── URL forwarding helpers ────────────────────────────────────────────────

    #[test]
    fn test_parse_url_scheme_authority_https() {
        let (scheme, authority) = parse_url_scheme_authority("https://api.example.com").unwrap();
        assert_eq!(scheme, "https");
        assert_eq!(authority, "api.example.com");
    }

    #[test]
    fn test_parse_url_scheme_authority_with_path() {
        let (scheme, authority) = parse_url_scheme_authority("https://api.example.com/v1/foo").unwrap();
        assert_eq!(scheme, "https");
        assert_eq!(authority, "api.example.com");
    }

    #[test]
    fn test_parse_url_scheme_authority_with_port() {
        let (scheme, authority) = parse_url_scheme_authority("https://api.example.com:8443/v1").unwrap();
        assert_eq!(scheme, "https");
        assert_eq!(authority, "api.example.com:8443");
    }

    #[test]
    fn test_parse_url_invalid_no_scheme() {
        assert!(parse_url_scheme_authority("api.example.com").is_err());
    }

    #[test]
    fn test_split_host_port_default() {
        let (host, port) = split_host_port("api.example.com", 443).unwrap();
        assert_eq!(host, "api.example.com");
        assert_eq!(port, 443);
    }

    #[test]
    fn test_split_host_port_explicit() {
        let (host, port) = split_host_port("api.example.com:8443", 443).unwrap();
        assert_eq!(host, "api.example.com");
        assert_eq!(port, 8443);
    }

    #[test]
    fn test_split_host_port_ipv6() {
        let (host, port) = split_host_port("[::1]:9000", 443).unwrap();
        assert_eq!(host, "::1");
        assert_eq!(port, 9000);
    }

    #[test]
    fn test_split_host_port_ipv6_default() {
        let (host, port) = split_host_port("[::1]", 443).unwrap();
        assert_eq!(host, "::1");
        assert_eq!(port, 443);
    }

    #[test]
    fn test_build_forwarded_request_bytes_basic() {
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/api/test".to_string(),
            query: Some("key=value".to_string()),
            version: "HTTP/3".to_string(),
            headers: vec![
                ("accept".to_string(), "application/json".to_string()),
                ("connection".to_string(), "keep-alive".to_string()), // hop-by-hop, should be stripped
            ],
            body: vec![],
        };
        let bytes = build_forwarded_request_bytes(&req, "api.example.com", "1.2.3.4", "original.host");
        let s = std::str::from_utf8(&bytes).unwrap();

        assert!(s.starts_with("GET /api/test?key=value HTTP/1.1\r\n"));
        assert!(s.contains("Host: api.example.com\r\n"));
        assert!(s.contains("accept: application/json\r\n"));
        // hop-by-hop 'connection' must not appear
        assert!(!s.contains("connection: keep-alive"));
        assert!(s.contains("X-Forwarded-For: 1.2.3.4\r\n"));
        assert!(s.contains("X-Forwarded-Host: original.host\r\n"));
        // Connection: close must be present
        assert!(s.contains("Connection: close\r\n"));
    }

    #[test]
    fn test_forward_request_timeout_conn_refused() {
        // Connecting to a port that has no listener should fail, not hang.
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/".to_string(),
            query: None,
            version: "HTTP/1.1".to_string(),
            headers: vec![],
            body: vec![],
        };
        // /tmp/nonexistent-m6-test.sock will not exist
        let path = std::path::Path::new("/tmp/nonexistent-m6-test-timeout.sock");
        let timeout = std::time::Duration::from_millis(100);
        let result = forward_request_timeout(path, &req, "127.0.0.1", "localhost", Some(timeout));
        assert!(result.is_err());
    }
}
