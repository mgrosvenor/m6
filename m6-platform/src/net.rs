//! Sockets with the options every m6 socket needs, and the connect results
//! normalised. The only place a socket is made.
//!
//! On macOS every socket made here has `SO_NOSIGPIPE`, so a write to a closed
//! peer is an error and never a signal. On Linux there is no such option and
//! none is needed: the Rust runtime ignores `SIGPIPE` in every binary, so the
//! same write is `BrokenPipe` on both. m6 does not set `SO_REUSEPORT` on any
//! listener, because Linux balances connections across the sockets that share
//! a port and macOS does not, so nothing may rely on it.
//!
//! The default socket buffer sizes differ (about 128 KiB on macOS, about
//! 208 KiB on Linux, both tunable by the host). A test that depends on a size
//! asks for it through [`set_buffer_sizes`].

use std::io;
use std::net::SocketAddr;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

pub use mio::net::{TcpListener, TcpStream, UdpSocket, UnixListener, UnixStream};

use super::os;

/// A non-blocking unix listener. Start only.
///
/// Removes a stale socket file left by a previous process, creates the parent
/// directory, binds and sets `mode`. A path at or over
/// [`super::MAX_SOCKET_PATH`] fails before the system call with the path and
/// the limit in the message. Every failure is an `Err` the caller turns into
/// exit 2: a socket with a mode nobody chose is a socket nobody can reason
/// about.
pub fn bind_unix(path: &Path, mode: u32) -> io::Result<UnixListener> {
    super::check_socket_path(path)?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let sock = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    os::prepare_socket(&sock)?;
    sock.bind(&socket2::SockAddr::unix(path)?)?;
    // The same backlog std uses.
    sock.listen(128)?;
    sock.set_nonblocking(true)?;
    // A socket whose mode cannot be set is a start failure: a mode nobody chose
    // is a socket nobody can reason about.
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(mode))?;
    Ok(UnixListener::from_std(
        std::os::unix::net::UnixListener::from(OwnedFd::from(sock)),
    ))
}

/// A non-blocking TCP listener with `SO_REUSEADDR`, so a restart rebinds a
/// port whose connections are in `TIME_WAIT` while a live listener on the port
/// is still refused. Start only.
pub fn listen_tcp(addr: SocketAddr) -> io::Result<TcpListener> {
    let sock = socket2::Socket::new(
        domain_of(addr),
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;
    os::prepare_socket(&sock)?;
    sock.set_reuse_address(true)?;
    sock.bind(&addr.into())?;
    sock.listen(128)?;
    sock.set_nonblocking(true)?;
    Ok(TcpListener::from_std(std::net::TcpListener::from(
        OwnedFd::from(sock),
    )))
}

/// A non-blocking connect to a unix listener. It completes at once or fails.
///
/// A refused connect (`ConnectionRefused`) means the socket file has no
/// listener: the member is dead, and a pool marks it failed. A path that does
/// not exist is `NotFound`. A listener whose queue is full is `WouldBlock` on
/// Linux (`EAGAIN`): alive and busy, so the caller answers busy and keeps the
/// member. On macOS the kernel answers `ECONNREFUSED` for a full queue as
/// well, and nothing tells the two apart, so there a full queue reads as a
/// failed member for one backoff. The error is the kernel's on both.
pub fn connect_unix(path: &Path) -> io::Result<std::os::unix::net::UnixStream> {
    super::check_socket_path(path)?;
    let sock = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    os::prepare_socket(&sock)?;
    sock.set_nonblocking(true)?;
    sock.connect(&socket2::SockAddr::unix(path)?)
        .map_err(os::map_connect_unix_error)?;
    Ok(OwnedFd::from(sock).into())
}

/// Whether a non-blocking TCP connect finished in the call or is still going.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Connect {
    /// Connected: the stream is usable now.
    Connected,
    /// In progress: wait for the stream to be writable, then ask
    /// `take_error` whether it connected.
    InProgress,
}

/// A non-blocking TCP connect with `TCP_NODELAY`. On both targets a connect
/// that cannot finish in the call reports `InProgress` (`EINPROGRESS`, which
/// `socket2` surfaces as `WouldBlock` on one and as the raw errno on the
/// other), and any other error is returned.
pub fn connect_tcp(addr: SocketAddr) -> io::Result<(std::net::TcpStream, Connect)> {
    let sock = socket2::Socket::new(
        domain_of(addr),
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;
    os::prepare_socket(&sock)?;
    sock.set_nonblocking(true)?;
    sock.set_tcp_nodelay(true)?;
    let state = match sock.connect(&socket2::SockAddr::from(addr)) {
        Ok(()) => Connect::Connected,
        Err(e) if connect_in_progress(&e) => Connect::InProgress,
        Err(e) => return Err(e),
    };
    Ok((OwnedFd::from(sock).into(), state))
}

/// `EINPROGRESS`, as `socket2` reports it on either target.
fn connect_in_progress(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::WouldBlock
        || e.raw_os_error() == Some(nix::errno::Errno::EINPROGRESS as i32)
}

/// Set the send and receive buffer sizes, in bytes. The kernel may round them
/// up, so a later read of the size is at least what was asked. For a test that
/// depends on a buffer size, because the platform defaults differ.
pub fn set_buffer_sizes(sock: &impl AsFd, send: usize, recv: usize) -> io::Result<()> {
    let s = socket2::SockRef::from(sock);
    s.set_send_buffer_size(send)?;
    s.set_recv_buffer_size(recv)
}

/// The send and receive buffer sizes the kernel holds for `sock`, in bytes.
pub fn buffer_sizes(sock: &impl AsFd) -> io::Result<(usize, usize)> {
    let s = socket2::SockRef::from(sock);
    Ok((s.send_buffer_size()?, s.recv_buffer_size()?))
}

/// The index of the network interface named `name`, for a link-local IPv6
/// scope. `NotFound` when there is no such interface.
pub fn interface_index(name: &str) -> io::Result<u32> {
    let idx = nix::net::if_::if_nametoindex(name)?;
    Ok(idx)
}

fn domain_of(addr: SocketAddr) -> socket2::Domain {
    match addr {
        SocketAddr::V4(_) => socket2::Domain::IPV4,
        SocketAddr::V6(_) => socket2::Domain::IPV6,
    }
}
