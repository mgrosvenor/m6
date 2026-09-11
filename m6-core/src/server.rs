/// Unix socket server for m6 inter-process communication.

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::io::Write;

use anyhow::Result;
use tracing::{debug, error, warn};

use crate::http::{RawRequest, RawResponse};
use crate::parse;

/// Derive the Unix socket path from a config file path.
///
/// Rule: basename, strip last extension, prepend /run/m6/
///
/// Examples:
///   "configs/m6-html.conf"   → "/run/m6/m6-html.sock"
///   "configs/m6-html-2.conf" → "/run/m6/m6-html-2.sock"
///   "/abs/path/to/foo.bar"   → "/run/m6/foo.sock"
///
/// # `M6_SOCKET_OVERRIDE`
///
/// Set, it wins outright. A test cannot write to `/run/m6`, so every service
/// needs an escape hatch, and every service had written the same one:
///
/// ```text
/// let socket_path = if let Ok(p) = std::env::var("M6_SOCKET_OVERRIDE") {
///     PathBuf::from(p)
/// } else {
///     socket_path_from_config(&config_path)
/// };
/// ```
///
/// Three byte-identical copies of that, in `app.rs`, `m6-file` and
/// `m6-auth-server`, wrapping this function rather than living in it. A fourth
/// copy of the *derivation* sat in `m6-file/src/config.rs`, which is what
/// m6-file actually called, and it differed: its fallback stem was `m6-file`
/// where this one is `m6-default`. Harmless, and exactly the shape that is not
/// harmless next time.
///
/// It lives here so the contract is one thing an app inherits rather than four
/// things an app remembers.
pub fn socket_path_from_config(config_path: &Path) -> PathBuf {
    if let Ok(override_path) = std::env::var("M6_SOCKET_OVERRIDE") {
        return PathBuf::from(override_path);
    }
    let stem = config_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("m6-default");
    PathBuf::from(format!("/run/m6/{}.sock", stem))
}

/// Default read timeout for an accepted connection, in seconds.
///
/// Not a fresh guess: `m6-file` and `m6-auth-server` had each hand-written
/// `set_read_timeout(Some(Duration::from_secs(30)))` into their own accept
/// path, so 30 is what this fleet already runs. The five services built on
/// [`crate::app::App`] had no timeout at all.
pub const DEFAULT_READ_TIMEOUT_SECS: u64 = 30;

/// Apply the per-connection read timeout, in one place.
///
/// A silent peer is the whole reason this exists. `serve_connection` blocks in
/// `read` waiting for a request line, so a peer that connects and never speaks
/// holds its worker until it goes away. With a bounded thread pool, a handful
/// of such peers is the entire pool, and the service answers 503 while looking
/// perfectly healthy: no error, no panic, no log line.
///
/// Failure to set the option is logged and tolerated rather than fatal. The
/// connection still works; it merely lacks a deadline, which is exactly where
/// every one of these services was before. Refusing to serve it would turn a
/// missing safety net into an outage.
pub fn apply_read_timeout(stream: &UnixStream, timeout: Option<std::time::Duration>) {
    let Some(dur) = timeout else { return };
    if let Err(e) = stream.set_read_timeout(Some(dur)) {
        warn!(error = %e, "failed to set read timeout on accepted connection");
    }
}

/// A running Unix socket server.
///
/// Accepts connections, calls handler for each, sends response.
pub struct UnixServer {
    path: PathBuf,
    listener: UnixListener,
}

impl UnixServer {
    /// Bind to the given socket path.  Removes a stale socket file first.
    pub fn bind(socket_path: PathBuf) -> Result<Self> {
        // Remove stale socket file if it exists.
        if socket_path.exists() {
            warn!(path = %socket_path.display(), "removing stale socket file");
            std::fs::remove_file(&socket_path)?;
        }

        // Ensure the directory exists.
        if let Some(parent) = socket_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let listener = UnixListener::bind(&socket_path)?;
        debug!(path = %socket_path.display(), "listening on Unix socket");

        Ok(UnixServer {
            path: socket_path,
            listener,
        })
    }

    /// Returns the socket path for logging.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Accept one connection, read request, call handler, write response.
    pub fn accept_one<F>(&self, handler: F) -> Result<()>
    where
        F: FnOnce(RawRequest) -> RawResponse,
    {
        let (mut stream, _addr) = self.listener.accept()?;
        handle_connection(&mut stream, handler);
        Ok(())
    }

    /// Returns the raw UnixListener for use in poll/select/epoll.
    pub fn listener(&self) -> &UnixListener {
        &self.listener
    }
}

fn handle_connection<F>(stream: &mut UnixStream, handler: F)
where
    F: FnOnce(RawRequest) -> RawResponse,
{
    let req = match parse::parse_request(stream) {
        Ok(r) => r,
        Err(e) => {
            error!(error = %e, "failed to parse request");
            let resp = RawResponse::new(e.status()).body(format!("{}: {}", e.reason(), e));
            let _ = stream.write_all(&resp.to_bytes());
            return;
        }
    };

    let resp = handler(req);
    let mut out = crate::h1::Responder::new(stream, "", false);
    if let Err(e) = resp.send(&mut out) {
        error!(error = %e, "failed to write response");
    }
}

/// Most requests one connection may serve before it is closed.
///
/// Persistent connections are the default in HTTP/1.1 (RFC 9112 9.3), but an
/// unbounded one lets a single peer hold a slot forever by pipelining. The
/// edge uses the same cap.
pub const MAX_REQUESTS_PER_CONN: u32 = 100;

/// Serve HTTP/1.1 on one accepted connection until it ends.
///
/// **This is the one backend connection loop.** m6-file, m6-html and
/// m6-auth-server each had their own, and each of the three answered exactly
/// one request and closed -- legal (RFC 9112 9.3 allows a server to close at
/// any time) and expensive, because m6-http speaks HTTP/1.1 to its backends
/// over unix sockets, so every cache miss paid a fresh connect and a fresh
/// accept. Measured by h1spec as "Keep-alive default (HTTP/1.1)" failing on
/// all three.
///
/// The handler is given a [`crate::h1::Responder`] rather than the stream, so
/// the two rules that must hold for every response -- no body on a HEAD, and
/// persistence decided here rather than by the handler -- cannot be forgotten
/// one branch at a time.
///
/// A malformed request is answered and the connection closed: after a framing
/// error there is no way to know where the next request starts, and guessing
/// is how a smuggled one gets through.
pub fn serve_connection<S, E, F>(stream: &mut S, mut handler: F) -> Result<(), E>
where
    S: std::io::Read + std::io::Write,
    E: From<std::io::Error>,
    F: FnMut(&RawRequest, &mut crate::h1::Responder<'_, S>) -> Result<(), E>,
{
    let mut served = 0u32;
    loop {
        let req = match parse::parse_request(stream) {
            Ok(r) => r,
            // An idle persistent connection going away is how this loop is
            // meant to end, not an error to report.
            Err(crate::parse::ParseError::ConnectionClosed) => return Ok(()),
            Err(e) => {
                debug!(error = %e, "malformed request");
                let mut resp = crate::h1::Responder::new(stream, "", false);
                resp.error(e.status())?;
                return Ok(());
            }
        };

        served += 1;
        let keep_alive = crate::h1::keep_alive(&req) && served < MAX_REQUESTS_PER_CONN;

        let mut resp = crate::h1::Responder::new(stream, &req.method, keep_alive);
        handler(&req, &mut resp)?;

        if !keep_alive {
            return Ok(());
        }
    }
}

impl Drop for UnixServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Framework-facing helpers, moved from m6-render in Phase 5.
// ---------------------------------------------------------------------------

/// Parse an HTTP/1.1 request from a Unix stream.
///
/// Delegates to the one parser, `crate::h1`, via its streaming wrapper.
/// This function used to contain a second implementation: its own request-line
/// and header reader over `BufReader::read_line`, scoring 15/32 on h1spec
/// against the shared parser's 27/32, with no cap on the request line or on
/// the number of headers.
///
/// Returns `None` if the connection closed without sending anything, which is
/// an idle keep-alive connection going away rather than an error.
pub fn parse_request(stream: &mut UnixStream) -> anyhow::Result<Option<RawRequest>> {
    match crate::parse::parse_request(stream) {
        Ok(req) => Ok(Some(req)),
        Err(crate::parse::ParseError::ConnectionClosed) => Ok(None),
        Err(e) => Err(anyhow::anyhow!(e)),
    }
}

/// Send a response through the one HTTP/1.1 response writer.
pub fn write_response<W: std::io::Write>(
    resp: &mut crate::h1::Responder<'_, W>,
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
    let mut resp = crate::h1::Responder::new(stream, "", false);
    resp.error(status)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_socket_path_from_config_relative() {
        let p = socket_path_from_config(Path::new("configs/m6-html.conf"));
        assert_eq!(p, PathBuf::from("/run/m6/m6-html.sock"));
    }

    #[test]
    fn test_socket_path_from_config_double_extension() {
        let p = socket_path_from_config(Path::new("configs/m6-html-2.conf"));
        assert_eq!(p, PathBuf::from("/run/m6/m6-html-2.sock"));
    }

    #[test]
    fn test_socket_path_from_config_absolute() {
        let p = socket_path_from_config(Path::new("/abs/path/to/foo.bar"));
        assert_eq!(p, PathBuf::from("/run/m6/foo.sock"));
    }

    #[test]
    fn test_socket_path_from_config_no_extension() {
        let p = socket_path_from_config(Path::new("configs/myconfig"));
        assert_eq!(p, PathBuf::from("/run/m6/myconfig.sock"));
    }

    #[test]
    fn test_parse_get_request() {
        let dir = tempfile::tempdir().unwrap();
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
        let dir = tempfile::tempdir().unwrap();
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
