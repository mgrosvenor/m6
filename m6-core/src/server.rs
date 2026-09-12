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

/// What one `poll(2)` wait on a listener plus a config watcher returned.
pub struct PollReady {
    /// The listener has at least one connection waiting to be accepted.
    pub listener: bool,
    /// The config watcher's fd fired. Its events still need draining, which is
    /// the caller's job because only the caller knows which filenames matter.
    pub watcher: bool,
    /// Neither fd fired: the timeout elapsed, or the call was interrupted.
    ///
    /// One flag for two outcomes because every caller treats them the same.
    /// `EINTR` here is a signal arriving during the wait, and the next thing
    /// both services do is check the shutdown flag, which is exactly the right
    /// response to that.
    pub idle: bool,
}

/// Wait for a connection or a config change, whichever comes first.
///
/// **This block was written twice.** `App` and `m6-file` each built a
/// `BorrowedFd` and a `PollFd` for the listener, branched on whether the
/// watcher had a usable fd, polled one or two descriptors with a 100 ms
/// timeout, and unpacked `revents` into a pair of bools. The two copies
/// differed only in local names and in whether an absent watcher fd was spelled
/// `Option<RawFd>` or `-1`.
///
/// The timeout is what makes an idle service still notice a shutdown promptly,
/// so it is the caller's to choose rather than baked in here.
///
/// What is deliberately *not* here is what the two services do next. One
/// submits to a bounded thread pool and answers 503 when it is full, the other
/// sends down a channel to a fixed worker set and counts in-flight requests
/// itself. Those are two concurrency models, not two copies of one, and
/// merging them is the separate piece of work in `CONSOLIDATION-TODO.md`
/// §3b-later.
pub fn poll_listener_and_watcher(
    listener_fd: std::os::fd::RawFd,
    watcher_fd: Option<std::os::fd::RawFd>,
    timeout_ms: u16,
) -> PollReady {
    use nix::poll::{poll, PollFd, PollFlags};
    use std::os::fd::BorrowedFd;

    // Safety: both fds are owned by the caller and outlive this call. They are
    // borrowed rather than owned precisely so that returning does not close
    // them.
    let borrowed_listener = unsafe { BorrowedFd::borrow_raw(listener_fd) };
    let mut pfd_listener = PollFd::new(&borrowed_listener, PollFlags::POLLIN);

    let fired = |pfd: &PollFd| {
        pfd.revents().is_some_and(|f| f.contains(PollFlags::POLLIN))
    };

    let timeout = timeout_ms as i32;
    match watcher_fd {
        Some(wfd) => {
            let borrowed_watcher = unsafe { BorrowedFd::borrow_raw(wfd) };
            let pfd_watcher = PollFd::new(&borrowed_watcher, PollFlags::POLLIN);
            let mut fds = [pfd_listener, pfd_watcher];
            let result = poll(&mut fds, timeout);
            PollReady {
                listener: fired(&fds[0]),
                watcher: fired(&fds[1]),
                idle: matches!(result, Ok(0) | Err(_)),
            }
        }
        None => {
            let result = poll(std::slice::from_mut(&mut pfd_listener), timeout);
            PollReady {
                listener: fired(&pfd_listener),
                watcher: false,
                idle: matches!(result, Ok(0) | Err(_)),
            }
        }
    }
}

/// Default mode for a service's unix socket.
///
/// `0o660`, not the `0o666` that `m6-file` and `m6-auth-server` each set by
/// hand. Every unit on this fleet runs `User=m6` and every socket lives in
/// `/run/m6`, which systemd creates `0750` and owns as `m6`, so the world bits
/// grant nothing that the directory does not already deny. They were free, and
/// a permission that is free today is the one nobody re-examines when the
/// directory mode changes.
///
/// Owner and group are what is actually used: the service creates the socket as
/// `m6` and every consumer connects as `m6`, so this is the boundary the fleet
/// already relies on, written down rather than inferred from a umask.
pub const DEFAULT_SOCKET_MODE: u32 = 0o660;

/// Apply the socket mode after bind, in one place.
///
/// `App` set no mode at all, so its five services took whatever the umask gave
/// them, typically `0o755`. That worked for the same reason `0o666` worked:
/// the directory was doing the enforcing. Neither is a decision anybody made.
///
/// Failure is logged and tolerated. The socket is already bound and the service
/// is already serving; refusing to continue would trade a socket that is more
/// permissive than intended for one that does not exist.
pub fn apply_socket_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
        warn!(error = %e, mode = format!("{mode:04o}"), "failed to set socket permissions");
    }
}

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
    response: crate::response::Response,
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
/// a genuine "something else is already running here" is still an error. That
/// is `SO_REUSEPORT`, which this deliberately does not set.
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
    /// error rather than two servers silently sharing a port. That would be
    /// `SO_REUSEPORT`, which this deliberately does not set.
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
