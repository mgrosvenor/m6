//! The parts of a service that are not its handlers.
//!
//! Deriving the unix socket path from the config path, binding a TCP listener
//! the way a server should, applying a socket mode, and the blocking stand-ins
//! that benchmarks and tests still drive: one HTTP/1.1 request off a Unix
//! stream and one response written to it. A service's loop uses
//! [`crate::h1::ServerConn`] for both.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use crate::log::debug;
use anyhow::Result;

use crate::http::RawRequest;
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
/// It lives here so the contract is one thing a service calls rather than a
/// rule each service rediscovers. A second copy of the derivation used a
/// different fallback stem, which is harmless today and exactly the shape that
/// is not harmless next time.
///
/// This exception is documented in `docs/m6-architecture.md`.
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
/// Not a fresh guess: `m6-file` and `m6-auth-server` each hand-wrote
/// `set_read_timeout(Some(Duration::from_secs(30)))` into their own accept
/// path, so 30 is the value already in service.
pub const DEFAULT_READ_TIMEOUT_SECS: u64 = 30;

/// Default mode for a service's unix socket.
///
/// `0o660`, not the `0o666` that `m6-file` and `m6-auth-server` each set by
/// hand. The intended arrangement is every unit under one service user with
/// its sockets in a directory that user owns, typically `0750`, where the
/// world bits grant nothing the directory does not already deny. They were
/// free, and a permission that is free today is the one nobody re-examines
/// when the directory mode changes.
///
/// Owner and group are the boundary that actually does the work: the service
/// creates the socket and its consumers connect as the same user. This writes
/// that down rather than leaving it inferred from a umask.
pub const DEFAULT_SOCKET_MODE: u32 = 0o660;

/// Apply the socket mode after bind, in one place.
///
/// Without it a socket takes whatever the umask gives, typically `0o755`, and
/// that works only while the directory is doing the enforcing. Neither is a
/// decision anybody made.
///
/// A socket whose mode cannot be set is a start failure: a mode nobody chose is
/// a socket nobody can reason about, so the caller stops rather than serve on
/// a socket that is more permissive than intended.
pub fn apply_socket_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Most requests one connection may serve before it is closed.
///
/// Persistent connections are the default in HTTP/1.1 (RFC 9112 9.3), but an
/// unbounded one lets a single peer hold a slot forever by pipelining. The
/// edge uses the same cap.
pub const MAX_REQUESTS_PER_CONN: u32 = 100;

/// Serve HTTP/1.1 on one accepted connection until it ends.
///
/// **This is the blocking reference loop the benchmarks and tests drive.**
/// Every service frames requests with [`crate::h1::ServerConn`]; this
/// function stays for the two benchmarks and the one test that read a socket
/// directly. It keeps the keep-alive rules one place, so a stand-in cannot
/// measure a different connection lifetime from the one the contract states.
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
    // Takes the request by value: this loop has no use for it after the
    // handler returns, and handing it over saves the handler a full clone of
    // the method, path, query, every header and the body, once per request,
    // purely so a `Request` could own one.
    F: FnMut(RawRequest, &mut crate::h1::Responder<'_, S>) -> Result<(), E>,
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
        handler(req, &mut resp)?;

        if !keep_alive {
            return Ok(());
        }
    }
}

// ---------------------------------------------------------------------------
// Blocking stand-ins
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
    response: crate::response::Response,
) -> anyhow::Result<()> {
    response.send(resp)
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixListener;

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
    fn a_chmod_that_fails_is_returned() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-socket");
        assert!(apply_socket_mode(&missing, 0o660).is_err());
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
        client
            .write_all(b"GET /test?foo=bar HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
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

/// Bind a TCP listener with `SO_REUSEADDR`, the way a server should.
///
/// **`std::net::TcpListener::bind` does not set it**, and m6-http did not set
/// it either, so a port carrying connections in `TIME_WAIT` could not be
/// rebound. That is not an edge case for a server: the peer that closes first
/// holds `TIME_WAIT`, and for an HTTP server sending `Connection: close` that
/// is us, on every closed connection, for up to a minute afterwards.
///
/// The consequence in production is worse than the one in the tests. m6-http
/// treats a failed bind as a warning and carries on with the listener set to
/// `None`, so a restart inside that window brings the process up **with
/// nothing listening on 443**, running, healthy to systemd, and serving no
/// one. The test suite only showed it as intermittent
/// `Address already in use (os error 48)` because the ports are recycled fast.
///
/// `SO_REUSEADDR` is the right tool and not a blunt one: it permits binding
/// over `TIME_WAIT`, and still refuses a port that has a **live** listener, so
/// a genuine "something else is already running here" is still an error.
pub fn bind_tcp_reuseaddr(addr: std::net::SocketAddr) -> std::io::Result<std::net::TcpListener> {
    let domain = match addr {
        std::net::SocketAddr::V4(_) => socket2::Domain::IPV4,
        std::net::SocketAddr::V6(_) => socket2::Domain::IPV6,
    };
    let sock = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    sock.set_reuse_address(true)?;
    sock.bind(&addr.into())?;
    // The same backlog std uses, so this changes one thing and not two.
    sock.listen(128)?;
    Ok(sock.into())
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "test code: threads, sleeps and blocking clients drive the service from outside"
)]
mod bind_tests {
    use super::bind_tcp_reuseaddr;

    fn loopback(port: u16) -> std::net::SocketAddr {
        std::net::SocketAddr::from(([127, 0, 0, 1], port))
    }

    /// The property that fixes the flake: a port whose previous listener is
    /// gone can be rebound at once, even with sockets left in `TIME_WAIT`.
    #[test]
    fn a_port_can_be_rebound_after_its_listener_and_a_connection_close() {
        let first = bind_tcp_reuseaddr(loopback(0)).expect("first bind");
        let port = first.local_addr().unwrap().port();

        // A real accepted connection, closed from the server side, which is
        // what leaves TIME_WAIT on this port.
        let client = std::net::TcpStream::connect(loopback(port)).expect("connect");
        let (server, _) = first.accept().expect("accept");
        drop(server);
        drop(client);
        drop(first);

        bind_tcp_reuseaddr(loopback(port))
            .expect("a port must be rebindable once its listener is gone");
    }

    /// And it is not a blunt instrument. A port with a **live** listener is
    /// still refused, so "something else is already running here" stays an
    /// error rather than two servers silently sharing a port.
    #[test]
    fn a_live_listener_still_refuses_a_second_bind() {
        let held = bind_tcp_reuseaddr(loopback(0)).expect("first bind");
        let port = held.local_addr().unwrap().port();
        assert!(
            bind_tcp_reuseaddr(loopback(port)).is_err(),
            "two live listeners on one port must not be allowed"
        );
        drop(held);
    }
}
