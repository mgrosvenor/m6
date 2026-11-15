//! Sockets with the options every m6 socket needs, and the connect results
//! normalised. The only place a socket is made.
//!
//! On macOS every socket made here has `SO_NOSIGPIPE`, so a write to a closed
//! peer is an error and never a signal. On Linux there is no such option and
//! none is needed: the Rust runtime ignores `SIGPIPE` in every binary, so the
//! same write is `BrokenPipe` on both. `SO_REUSEPORT` is set only by
//! [`listen_shared`], which is what lets several processes serve one port.
//!
//! The default socket buffer sizes differ (about 128 KiB on macOS, about
//! 208 KiB on Linux, both tunable by the host). A test that depends on a size
//! asks for it through [`set_buffer_sizes`].

use std::io;
use std::net::SocketAddr;
use std::os::fd::{AsFd, OwnedFd, RawFd};
use std::path::Path;

pub use mio::net::{TcpListener, TcpStream, UdpSocket, UnixListener, UnixStream};

use super::os;

/// A non-blocking unix listener. Start only.
///
/// A socket file left by a process that is gone is removed, the parent
/// directory is created, the socket is bound and `mode` set. A socket file
/// something still listens on is `AddrInUse` naming the path: a second
/// instance never unlinks a live listener's socket. A path that holds
/// something other than a socket is `AlreadyExists` naming what is there. A
/// path at or over [`super::MAX_SOCKET_PATH`] fails before the system call
/// with the path and the limit in the message. Every failure is an `Err` the
/// caller turns into exit 2.
pub fn bind_unix(path: &Path, mode: u32) -> io::Result<UnixListener> {
    use std::os::unix::fs::FileTypeExt;
    super::check_socket_path(path)?;
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if !meta.file_type().is_socket() {
            let what = if meta.is_dir() {
                "a directory"
            } else {
                "a file"
            };
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is {what}, not a socket", path.display()),
            ));
        }
        match connect_unix(path) {
            // Answered, or alive and busy: someone listens there.
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("another process listens on {}", path.display()),
                ))
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("another process listens on {}", path.display()),
                ))
            }
            // Nobody listens: a stale file from a process that is gone.
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                std::fs::remove_file(path)?;
            }
            Err(e) => return Err(e),
        }
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

/// A non-blocking TCP listener and UDP socket bound to one address with
/// `SO_REUSEPORT`, so several processes serve the same public port. Start only.
///
/// The option is set on each socket before its `bind`, which is the only point
/// where it has an effect. Linux spreads a new TCP connection across the
/// sockets that share a port by a hash of its 4-tuple and gives every UDP
/// datagram to one socket. macOS spreads neither: the socket bound last to the
/// port receives every connection and every datagram, so a second process there
/// is a takeover rather than a scale-out. Nothing may assume an even spread on
/// either target, and on Linux a QUIC client that changes address can land on
/// another process.
///
/// m6-http opens it only for the public 443 port's TLS listener and its QUIC
/// UDP socket, and for no other socket.
///
/// The TCP socket also carries `SO_REUSEADDR`, as [`listen_tcp`] does, so a
/// restart rebinds a port whose connections are in `TIME_WAIT`. The returned
/// pair are `std` sockets because the service's loop registers their
/// descriptors itself.
pub fn listen_shared(addr: SocketAddr) -> io::Result<(std::net::TcpListener, std::net::UdpSocket)> {
    let tcp = socket2::Socket::new(
        domain_of(addr),
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;
    os::prepare_socket(&tcp)?;
    tcp.set_reuse_address(true)?;
    tcp.set_reuse_port(true)?;
    tcp.bind(&addr.into())?;
    tcp.listen(128)?;
    tcp.set_nonblocking(true)?;

    let udp = socket2::Socket::new(
        domain_of(addr),
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    os::prepare_socket(&udp)?;
    udp.set_reuse_port(true)?;
    udp.bind(&addr.into())?;
    udp.set_nonblocking(true)?;

    Ok((
        std::net::TcpListener::from(OwnedFd::from(tcp)),
        std::net::UdpSocket::from(OwnedFd::from(udp)),
    ))
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
    sock.connect(&socket2::SockAddr::unix(path)?)?;
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

/// What an `accept` error means for an edge-triggered accept loop.
///
/// The loop drains the listener to [`Accept::Drained`]. [`Accept::SkipOne`]
/// is a connection that went away before it was accepted (`ECONNABORTED`) or
/// an interrupted call: keep draining. [`Accept::Stop`] is any other failure:
/// log it once and stop accepting for this turn.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Accept {
    /// `WouldBlock`: the queue is empty.
    Drained,
    /// The connection is gone or the call was interrupted: accept the next.
    SkipOne,
    /// Any other failure: log it and stop accepting for this turn.
    Stop,
}

/// Classify an `accept` error. The same answer on every target.
pub fn accept_error(e: &io::Error) -> Accept {
    use nix::errno::Errno;
    match e.kind() {
        io::ErrorKind::WouldBlock => Accept::Drained,
        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted => Accept::SkipOne,
        _ if e.raw_os_error() == Some(Errno::ECONNABORTED as i32) => Accept::SkipOne,
        _ => Accept::Stop,
    }
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

/// Ask the kernel to report a socket writable only while its send queue holds
/// fewer than `bytes`, so a whole response body is not handed over at once and
/// the order m6 frames it in is the order it leaves.
///
/// Linux sets `TCP_NOTSENT_LOWAT`. macOS has no equivalent, so its kernel order
/// stands and this is a no-op. A kernel that will not set the option still
/// sends everything, in its own order, so the caller logs the error rather than
/// refusing the connection.
pub fn limit_send_queue(fd: RawFd, bytes: usize) -> io::Result<()> {
    super::os::limit_send_queue(fd, bytes)
}
