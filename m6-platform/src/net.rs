//! Sockets with the options every m6 socket needs, and the connect results
//! normalised. The only place a socket is made.
//!
//! On macOS every socket made here has `SO_NOSIGPIPE`, so a write to a closed
//! peer is an error and never a signal. On Linux there is no such option and
//! none is needed: the Rust runtime ignores `SIGPIPE` in every binary, so the
//! same write is `BrokenPipe` on both. `SO_REUSEPORT` is set only by
//! [`listen_shared`], which is what lets several processes serve one port and,
//! on Linux, keeps each client address on one of them.
//!
//! The default socket buffer sizes differ (about 128 KiB on macOS, about
//! 208 KiB on Linux, both tunable by the host). A test that depends on a size
//! asks for it through [`set_buffer_sizes`].

use std::io;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
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
/// With a port of 0 the kernel picks one for the TCP listener, and the UDP
/// socket is bound to that same port.
///
/// The option is set on each socket before its `bind`, which is the only point
/// where it has an effect. Left alone, Linux spreads a new TCP connection
/// across the sockets that share a port by a hash of its 4-tuple and gives
/// every UDP datagram to one socket by a hash of its addresses and ports, so
/// one client's connections and a client that changes port can land on
/// different processes. macOS spreads neither: the socket bound last to the
/// port receives every connection and every datagram, so a second process
/// there is a takeover rather than a scale-out, and one client always reaches
/// one process. Nothing may assume an even spread on either target.
///
/// `group_size` is how many processes share the port. With `Some(n)` on Linux,
/// a classic BPF program (`SO_ATTACH_REUSEPORT_CBPF`) is attached to both
/// sockets so that a client's source address alone picks the socket: (a hash of
/// the address) mod `n`. Every TCP connection and every QUIC datagram from one
/// address, IPv4 or IPv6, then reaches one process whatever its source port.
/// The program reads the address with the `SKF_NET_OFF` offsets and costs the
/// accept path no allocation. On macOS `group_size` has no effect, because
/// there is nothing to steer. With `None` nothing is attached on either target.
///
/// The index is the socket's place in the kernel's group, and `n` is what the
/// caller says the group will hold, not what it holds now. While fewer than `n`
/// sockets are in the group, a client whose index has no socket is placed by
/// the kernel's own hash. When a process stops or starts, sockets leave or
/// join the group and the kernel moves one to fill a vacated place, so some
/// clients change process until the group is whole again. Clients on a
/// surviving process are not all kept on it. A connection already accepted
/// stays where it is, and a QUIC client that moves process loses its
/// connection and handshakes again.
///
/// m6-http opens it only for the public 443 port's TLS listener and its QUIC
/// UDP socket, and for no other socket.
///
/// The TCP socket also carries `SO_REUSEADDR`, as [`listen_tcp`] does, so a
/// restart rebinds a port whose connections are in `TIME_WAIT`. The returned
/// pair are `std` sockets because the service's loop registers their
/// descriptors itself.
pub fn listen_shared(
    addr: SocketAddr,
    group_size: Option<NonZeroU32>,
) -> io::Result<(std::net::TcpListener, std::net::UdpSocket)> {
    let tcp = socket2::Socket::new(
        domain_of(addr),
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;
    os::prepare_socket(&tcp)?;
    tcp.set_reuse_address(true)?;
    tcp.set_reuse_port(true)?;
    tcp.bind(&addr.into())?;
    // A port of 0 asks the kernel for one. The UDP socket takes the TCP
    // listener's port, not a port of its own: sockets only share a reuseport
    // group on one address, and the two halves are one public port.
    let addr = tcp.local_addr()?.as_socket().unwrap_or(addr);
    tcp.listen(128)?;
    tcp.set_nonblocking(true)?;
    if let Some(n) = group_size {
        // After `listen`, which is when a TCP socket joins the group.
        os::steer_reuseport(tcp.as_raw_fd(), n.get())?;
    }

    let udp = socket2::Socket::new(
        domain_of(addr),
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    os::prepare_socket(&udp)?;
    udp.set_reuse_port(true)?;
    udp.bind(&addr.into())?;
    udp.set_nonblocking(true)?;
    if let Some(n) = group_size {
        os::steer_reuseport(udp.as_raw_fd(), n.get())?;
    }

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

/// Send `segments` one datagram at a time, in order.
///
/// Every datagram before the last is `segment_size` bytes and the last is at
/// most that, which is the shape a QUIC flush produces. This is the path a
/// socket without segmentation offload takes, on either target: Linux falls
/// back to it for a socket that has no `UDP_SEGMENT`, and macOS has nothing
/// else.
pub(super) fn udp_send_each(
    socket: &impl AsFd,
    segments: &[u8],
    segment_size: usize,
    dest: SocketAddr,
) -> io::Result<usize> {
    use nix::sys::socket::{sendto, MsgFlags, SockaddrStorage};

    let dst = SockaddrStorage::from(dest);
    let fd = socket.as_fd().as_raw_fd();
    let mut sent = 0;
    while sent < segments.len() {
        let len = (segments.len() - sent).min(segment_size);
        let wrote = sendto(fd, &segments[sent..sent + len], &dst, MsgFlags::empty())?;
        if wrote != len {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "a UDP send took part of a datagram",
            ));
        }
        sent += wrote;
    }
    Ok(sent)
}

/// The most datagrams one [`UdpBatch::recv`] call takes in.
pub const MAX_BATCH: usize = 16;

/// One datagram a batch receive took in: the bytes stored for it and the
/// address it came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecvMeta {
    /// Bytes stored in the caller's buffer for this datagram.
    pub len: usize,
    /// The peer address the datagram came from. `None` only when the kernel
    /// reported no address, which a UDP receive does not do.
    pub from: Option<SocketAddr>,
}

/// A socket's UDP batch send and receive.
///
/// One [`send`](Self::send) sends a buffer packed with segments of one size.
/// One [`recv`](Self::recv) takes in up to [`MAX_BATCH`] datagrams. On Linux
/// the send is one `sendmsg` carrying the segment size as a `UDP_SEGMENT`
/// control message, so the kernel splits the buffer into the datagrams the
/// caller packed, and the receive is one `recvmmsg`. On macOS there is no
/// segmentation offload and no `recvmmsg`: the same calls send and receive one
/// datagram at a time, in the order given, with the same bytes and the same
/// results.
///
/// [`for_socket`](Self::for_socket) asks the kernel once, at socket setup,
/// whether segmentation offload is available. A socket without it sends a
/// batch as single datagrams. [`without_gso`](Self::without_gso) is that path
/// on a socket that has it, so a test can hold both paths to the same bytes on
/// one machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UdpBatch {
    gso: bool,
}

impl UdpBatch {
    /// Probe `socket` once, at setup, for segmentation offload.
    pub fn for_socket(socket: &impl AsFd) -> UdpBatch {
        UdpBatch {
            gso: super::os::udp_gso_supported(socket),
        }
    }

    /// The path a socket without segmentation offload takes, on any socket.
    pub fn without_gso() -> UdpBatch {
        UdpBatch { gso: false }
    }

    /// Whether this socket sends a batch as one segmented `sendmsg`.
    pub fn gso(&self) -> bool {
        self.gso
    }

    /// Send `segments`, packed back to back, to `dest`.
    ///
    /// Every segment is exactly `segment_size` bytes and the last is at most
    /// `segment_size`, which is the shape a QUIC flush produces. Returns the
    /// bytes sent, which is `segments.len()`. A batch of one segment, or a
    /// socket without segmentation offload, sends one datagram per segment.
    pub fn send(
        &self,
        socket: &impl AsFd,
        segments: &[u8],
        segment_size: usize,
        dest: SocketAddr,
    ) -> io::Result<usize> {
        if segments.is_empty() || segment_size == 0 || segment_size > segments.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a batch needs a segment size within its bytes",
            ));
        }
        super::os::udp_send_batch(socket, segments, segment_size, dest, self.gso)
    }

    /// Receive up to [`MAX_BATCH`] datagrams into `buf`, one per `slot` bytes,
    /// storing datagram `i` at `buf[i * slot..(i + 1) * slot]` and its length
    /// and source in `metas[i]`.
    ///
    /// Takes in at most `metas.len()` datagrams and at most `buf.len() / slot`.
    /// `WouldBlock` means no datagram was waiting. A datagram longer than
    /// `slot` is truncated to the slot, as one `recv_from` would truncate it.
    /// A batch never drops a datagram it has read: an error met after a
    /// datagram returns that count. Whether the same error is reported again
    /// is the platform's. Linux stores it back on the socket and the next call
    /// reports it; macOS clears it where `recvfrom` reports it, and the next
    /// call reads the next datagram.
    pub fn recv(
        &self,
        socket: &impl AsFd,
        buf: &mut [u8],
        slot: usize,
        metas: &mut [RecvMeta],
    ) -> io::Result<usize> {
        if slot == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a receive batch needs a nonzero slot",
            ));
        }
        // The one place the batch count is worked out. The system call gets
        // slices already trimmed to it, so it has one count to trust and no
        // count of its own to get wrong.
        let count = (buf.len() / slot).min(metas.len()).min(MAX_BATCH);
        if count == 0 {
            return Ok(0);
        }
        super::os::udp_recv_batch(socket, &mut buf[..count * slot], slot, &mut metas[..count])
    }
}

/// The address a received datagram came from, `None` for an address family m6
/// does not use.
///
/// `SockaddrStorage` checks the family and the length before it hands out the
/// inner structure, so this replaces the hand-written cast that used to repeat
/// both checks in `unsafe`.
pub(super) fn socket_addr_of(address: &nix::sys::socket::SockaddrStorage) -> Option<SocketAddr> {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};

    if let Some(inet) = address.as_sockaddr_in() {
        let inet: &nix::libc::sockaddr_in = inet.as_ref();
        return Some(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::from(inet.sin_addr.s_addr.to_ne_bytes()),
            u16::from_be(inet.sin_port),
        )));
    }
    let inet6: &nix::libc::sockaddr_in6 = address.as_sockaddr_in6()?.as_ref();
    Some(SocketAddr::V6(SocketAddrV6::new(
        Ipv6Addr::from(inet6.sin6_addr.s6_addr),
        u16::from_be(inet6.sin6_port),
        inet6.sin6_flowinfo,
        inet6.sin6_scope_id,
    )))
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
