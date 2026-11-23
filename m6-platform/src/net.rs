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
/// an interrupted call: keep draining. [`Accept::Exhausted`] is `EMFILE` or
/// `ENFILE`: take the queued connection with the reserve descriptor
/// ([`SpareDescriptor`]), refuse it, and keep draining. [`Accept::Stop`] is
/// any other failure: log it once, leave the listener armed and stop
/// draining.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Accept {
    /// `WouldBlock`: the queue is empty.
    Drained,
    /// The connection is gone or the call was interrupted: accept the next.
    SkipOne,
    /// Out of descriptors (`EMFILE`, `ENFILE`): refuse the queued connection
    /// through the reserve and accept the next.
    Exhausted,
    /// Any other failure: log it, leave the listener armed, stop draining.
    Stop,
}

/// Classify an `accept` error. The same answer on every target.
pub fn accept_error(e: &io::Error) -> Accept {
    use nix::errno::Errno;
    match e.kind() {
        io::ErrorKind::WouldBlock => Accept::Drained,
        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted => Accept::SkipOne,
        _ if e.raw_os_error() == Some(Errno::ECONNABORTED as i32) => Accept::SkipOne,
        _ if matches!(
            e.raw_os_error(),
            Some(n) if n == Errno::EMFILE as i32 || n == Errno::ENFILE as i32
        ) =>
        {
            Accept::Exhausted
        }
        _ => Accept::Stop,
    }
}

/// What one [`SpareDescriptor::reject_one`] call did with the queued
/// connection.
#[derive(Debug)]
pub enum RejectOutcome {
    /// The connection was taken and closed: the service refused it, and it is
    /// counted.
    Refused,
    /// Nothing was taken: the queue was already empty, or the kernel reset the
    /// connection before `accept` could take it. Nothing was refused.
    Nothing,
    /// `accept` failed for a reason that is not a refusal. The reserve is held
    /// again, and the caller must log this rather than count it.
    Failed(io::Error),
}

/// The descriptor an accept loop keeps in reserve, so that running out of
/// descriptors refuses the queued connection instead of stalling the listener.
///
/// It is opened at start, before any pressure, and it is of the same socket
/// family as the listener it serves, so the descriptor the kernel hands back
/// is one this process can always make again.
///
/// On `EMFILE` or `ENFILE` the loop closes the reserve, takes the queued
/// connection in the descriptor that frees, closes that connection at once,
/// and opens the reserve again. That is `libuv`'s `uv__emfile_trick`, and it
/// is what keeps the listener draining. An edge-triggered listener has already
/// consumed the readiness that woke it, so the only thing that can wake it
/// again is another connection, and a full queue lets no other connection
/// complete: a loop that stops accepting there waits for the life of the
/// process. Taking and refusing the queued connection empties the queue, so
/// the turn ends with nothing unread and no re-arm to get wrong.
#[derive(Debug)]
pub struct SpareDescriptor {
    domain: socket2::Domain,
    fd: Option<OwnedFd>,
}

impl SpareDescriptor {
    /// A reserve for a listener of `domain`.
    pub fn new(domain: socket2::Domain) -> io::Result<Self> {
        let mut spare = SpareDescriptor { domain, fd: None };
        spare.fd = Some(spare.open()?);
        Ok(spare)
    }

    /// A reserve for a TCP listener bound at `local`, on that address's family.
    pub fn for_tcp(local: SocketAddr) -> io::Result<Self> {
        Self::new(domain_of(local))
    }

    /// A reserve for a unix listener.
    pub fn for_unix() -> io::Result<Self> {
        Self::new(socket2::Domain::UNIX)
    }

    /// Take and close the connection the kernel has queued, using the reserve.
    ///
    /// The reserve is closed first, then `accept` runs (it must take one
    /// connection and close it), then the reserve is opened again. `accept`
    /// therefore runs while the reserve's descriptor is free, which is the
    /// descriptor the queued connection is taken in.
    ///
    /// The reserve is held again on every return except the one error it can
    /// report, the reserve failing to reopen. That error is the caller's to
    /// handle, because the next shortage cannot be.
    ///
    /// The accept's own outcome is returned rather than discarded. An accept
    /// loop writes a line per refusal and the count must be exact, so
    /// [`RejectOutcome::Refused`] is the only outcome a caller may count:
    /// an emptied queue (`EAGAIN`) or a connection the kernel reset before
    /// `accept` could take it (`ECONNABORTED`/`ECONNRESET`) is
    /// [`RejectOutcome::Nothing`], and any other error is
    /// [`RejectOutcome::Failed`] and must reach the log instead of being
    /// counted as a refusal.
    pub fn reject_one(
        &mut self,
        accept: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<RejectOutcome> {
        // Dropping the reserve closes it: the queued connection needs the
        // descriptor number it held.
        self.fd = None;
        let taken = accept();
        let fd = self.open()?;
        self.fd = Some(fd);
        Ok(match taken {
            Ok(()) => RejectOutcome::Refused,
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::ConnectionAborted
                    || e.kind() == io::ErrorKind::ConnectionReset =>
            {
                RejectOutcome::Nothing
            }
            Err(e) => RejectOutcome::Failed(e),
        })
    }

    fn open(&self) -> io::Result<OwnedFd> {
        let sock = socket2::Socket::new(self.domain, socket2::Type::STREAM, None)?;
        os::prepare_socket(&sock)?;
        Ok(OwnedFd::from(sock))
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
///
/// Returns the bytes the socket took. A socket with no room for the next
/// datagram stops the send with `Ok` and a count below `segments.len()`, so a
/// caller retries only the datagrams that did not go out. Anything else that
/// stops the send is an `Err`.
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
        #[cfg(test)]
        let result = if test_send_blocked::due(sent) {
            Err(nix::errno::Errno::EAGAIN)
        } else {
            sendto(fd, &segments[sent..sent + len], &dst, MsgFlags::empty())
        };
        #[cfg(not(test))]
        let result = sendto(fd, &segments[sent..sent + len], &dst, MsgFlags::empty());
        let wrote = match result {
            Ok(wrote) => wrote,
            // The datagrams before this one are on the wire. Reporting the
            // count keeps them from being sent again by the retry.
            Err(e) if io::Error::from(e).kind() == io::ErrorKind::WouldBlock => return Ok(sent),
            Err(e) => return Err(e.into()),
        };
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

/// A socket that fills part way through a batch, forced by a test.
///
/// A loopback socket's send buffer cannot be made to fill, so the one
/// condition the partial send exists for has to be produced here. A release
/// build compiles neither this module nor its call.
#[cfg(test)]
mod test_send_blocked {
    use std::cell::Cell;

    thread_local! {
        /// Refuse a send once this many bytes have gone, on this thread.
        static AFTER: Cell<Option<usize>> = const { Cell::new(None) };
    }

    /// Refuse this thread's next send once `bytes` have been taken.
    pub(super) fn fail_after_bytes(bytes: usize) {
        AFTER.with(|a| a.set(Some(bytes)));
    }

    /// Whether this send is the one to refuse, taking the request off.
    pub(super) fn due(sent: usize) -> bool {
        AFTER.with(|a| match a.get() {
            Some(after) if sent >= after => {
                a.set(None);
                true
            }
            _ => false,
        })
    }
}

/// The most receives one [`UdpBatch::recv`] call takes in.
///
/// A receive is one datagram on a socket without receive coalescing. On Linux
/// with `UDP_GRO` enabled the kernel may hand back one buffer holding a run of
/// datagrams from one sender, and [`RecvMeta::datagrams`] splits that run.
pub const MAX_BATCH: usize = 16;

/// The bytes one receive slot must hold to take a coalesced read whole.
///
/// Linux UDP GRO coalesces the datagrams from one sender into one buffer of up
/// to 64 KB, the largest UDP payload, so a smaller slot would truncate it. A
/// socket without coalescing writes one datagram per slot, and a QUIC packet is
/// far below this, so the room is unused there.
pub const RECV_SLOT: usize = 64 * 1024;

/// Whether this target has UDP receive coalescing at all.
///
/// Linux has it and macOS does not. A Linux socket still asks for it, and a
/// kernel older than the option refuses, so [`UdpBatch::gro`] is the answer for
/// one socket.
pub const RECV_COALESCING: bool = super::os::RECV_COALESCING;

/// One receive a batch took in: the bytes stored for it, the address they came
/// from, and how to split them back into datagrams.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecvMeta {
    /// Bytes stored in the caller's buffer for this receive.
    ///
    /// With a zero [`stride`](Self::stride) this is one datagram. Otherwise the
    /// kernel coalesced several datagrams from one sender into this many bytes,
    /// and [`datagrams`](Self::datagrams) gives each of them.
    pub len: usize,
    /// The peer address the datagram came from. `None` only when the kernel
    /// reported no address, which a UDP receive does not do. Every datagram in
    /// a coalesced receive came from this one address.
    pub from: Option<SocketAddr>,
    /// Zero when the receive is one datagram. Otherwise the size of every
    /// datagram in it but the last, which may be shorter: the `gso_size` the
    /// kernel reported for a `UDP_GRO` read.
    pub stride: usize,
}

impl RecvMeta {
    /// The datagrams this receive holds, as `(offset, length)` within the slot
    /// the caller gave the receive.
    ///
    /// A receive that was not coalesced is one entry of `(0, len)`, including
    /// the zero-length datagram UDP allows. A coalesced one is `stride` bytes
    /// at a time, in the order the sender sent them, with the last at most
    /// `stride`.
    pub fn datagrams(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        let len = self.len;
        let stride = self.stride;
        let mut offset = 0;
        let mut sent_empty = false;
        std::iter::from_fn(move || {
            if offset < len {
                let width = if stride == 0 { len } else { stride };
                let end = len.min(offset.saturating_add(width));
                let at = offset;
                offset = end;
                return Some((at, end - at));
            }
            // A zero-length datagram is one datagram, not none.
            if len == 0 && !sent_empty {
                sent_empty = true;
                return Some((0, 0));
            }
            None
        })
    }
}

/// A socket's UDP batch send and receive.
///
/// One [`send`](Self::send) sends a buffer packed with segments of one size.
/// One [`recv`](Self::recv) takes in up to [`MAX_BATCH`] receives. On Linux
/// the send is one `sendmsg` carrying the segment size as a `UDP_SEGMENT`
/// control message, so the kernel splits the buffer into the datagrams the
/// caller packed, and the receive is one `recvmmsg`. On macOS there is no
/// segmentation offload and no `recvmmsg`: the same calls send and receive one
/// datagram at a time, in the order given, with the same bytes and the same
/// results.
///
/// On Linux a receive can also be coalesced. With `UDP_GRO` on, the kernel
/// hands back one buffer holding a run of datagrams from one sender, and
/// reports the run's `gso_size` in a control message. [`RecvMeta`] carries that
/// size as `stride`, and [`RecvMeta::datagrams`] splits the run into the
/// datagrams that were sent. macOS has no such option, and every receive there
/// is one datagram.
///
/// [`for_socket`](Self::for_socket) asks the kernel once, at socket setup,
/// whether either offload is available. A socket without segmentation offload
/// sends a batch as single datagrams, and a socket without receive coalescing
/// reads one datagram per call. [`without_gso`](Self::without_gso) is the send
/// path on a socket that has segmentation offload, so a test can hold both
/// paths to the same bytes on one machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UdpBatch {
    gso: bool,
    gro: bool,
}

impl UdpBatch {
    /// Ask `socket` once, at setup, for both offloads.
    ///
    /// Segmentation offload is probed and left off, so an ordinary send stays
    /// one datagram and only a batch asks the kernel to split one buffer.
    /// Receive coalescing is enabled and left on where the kernel accepts it,
    /// because every receive then reports what it took in. A kernel that
    /// refuses it is not an error: the socket reads one datagram at a time,
    /// which is the path it took before.
    pub fn for_socket(socket: &impl AsFd) -> UdpBatch {
        UdpBatch {
            gso: super::os::udp_gso_supported(socket),
            gro: super::os::udp_gro_supported(socket),
        }
    }

    /// The path a socket without segmentation offload takes, on any socket.
    pub fn without_gso() -> UdpBatch {
        UdpBatch {
            gso: false,
            gro: false,
        }
    }

    /// Whether this socket sends a batch as one segmented `sendmsg`.
    pub fn gso(&self) -> bool {
        self.gso
    }

    /// Whether this socket coalesces several datagrams into one receive.
    pub fn gro(&self) -> bool {
        self.gro
    }

    /// Send `segments`, packed back to back, to `dest`.
    ///
    /// Every segment is exactly `segment_size` bytes and the last is at most
    /// `segment_size`, which is the shape a QUIC flush produces. A batch of one
    /// segment, or a socket without segmentation offload, sends one datagram
    /// per segment.
    ///
    /// Returns the bytes the socket took. It is below `segments.len()` only
    /// when the socket refused the next datagram with `WouldBlock` part way
    /// through the batch, and then the caller sends only the rest.
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

    /// Receive up to [`MAX_BATCH`] receives into `buf`, one per `slot` bytes,
    /// storing receive `i` at `buf[i * slot..(i + 1) * slot]` and its length,
    /// source and split in `metas[i]`.
    ///
    /// A receive is one datagram whose length is `metas[i].len`, or on Linux a
    /// coalesced run of datagrams that [`RecvMeta::datagrams`] splits. Size the
    /// slot to [`RECV_SLOT`] so a coalesced run arrives whole.
    ///
    /// Takes in at most `metas.len()` receives and at most `buf.len() / slot`.
    /// `WouldBlock` means no datagram was waiting. A receive longer than `slot`
    /// is truncated to the slot, as one `recv_from` would truncate it.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Descriptor exhaustion is its own answer, so an accept loop can refuse
    /// the queued connection instead of stopping.
    ///
    /// Break: `EMFILE` and `ENFILE` classified as `Stop`, which leaves the
    /// listener edge-triggered with a connection queued and no wakeup left.
    #[test]
    fn descriptor_exhaustion_is_its_own_accept_answer() {
        use nix::errno::Errno;
        for errno in [Errno::EMFILE, Errno::ENFILE] {
            let e = io::Error::from_raw_os_error(errno as i32);
            assert_eq!(accept_error(&e), Accept::Exhausted, "{errno}");
        }
        // The three answers that were already classified keep theirs.
        assert_eq!(
            accept_error(&io::Error::from(io::ErrorKind::WouldBlock)),
            Accept::Drained
        );
        assert_eq!(
            accept_error(&io::Error::from(io::ErrorKind::Interrupted)),
            Accept::SkipOne
        );
        assert_eq!(
            accept_error(&io::Error::from_raw_os_error(Errno::ECONNABORTED as i32)),
            Accept::SkipOne
        );
        assert_eq!(
            accept_error(&io::Error::from_raw_os_error(Errno::EBADF as i32)),
            Accept::Stop
        );
    }

    /// The reserve is a stream socket of the listener's own family, held from
    /// start, and spending it takes a queued connection and holds it again.
    ///
    /// Break: a reserve opened as a file rather than a socket of the same
    /// family, or one that is not reopened after it is spent (the next
    /// shortage then cannot be handled at all).
    #[test]
    fn the_reserve_is_a_socket_of_the_listeners_family() {
        use nix::sys::socket::{getsockname, AddressFamily, SockaddrLike};

        let mut unix = SpareDescriptor::for_unix().expect("a unix reserve");
        let tcp = SpareDescriptor::for_tcp("127.0.0.1:9".parse().expect("an address"))
            .expect("a tcp reserve");
        let family = |spare: &SpareDescriptor| {
            let fd = spare.fd.as_ref().expect("a held reserve");
            getsockname::<nix::sys::socket::SockaddrStorage>(fd.as_raw_fd())
                .expect("the reserve's address")
                .family()
                .expect("a family")
        };
        assert_eq!(family(&unix), AddressFamily::Unix, "a unix reserve");
        assert_eq!(family(&tcp), AddressFamily::Inet, "a TCP reserve");
        let fd = unix.fd.as_ref().expect("a held reserve");
        assert_eq!(
            socket2::SockRef::from(fd)
                .r#type()
                .expect("the socket type"),
            socket2::Type::STREAM,
            "the reserve is a stream socket, so the descriptor it frees is the \
             one an accepted connection needs"
        );

        // Spending it runs the caller's accept with the reserve's descriptor
        // free, and holds the reserve again afterwards.
        let mut ran = false;
        let outcome = unix.reject_one(|| {
            ran = true;
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        });
        assert!(ran, "the caller's accept ran with the reserve free");
        assert!(
            matches!(outcome, Ok(RejectOutcome::Nothing)),
            "an emptied queue refused nothing, and the reserve was taken back"
        );
        assert!(unix.fd.is_some(), "held again after it was spent");
    }

    /// The reserve reports what the accept did: a taken connection is a
    /// refusal and is counted; an emptied queue or a connection the kernel
    /// reset before it could be taken is not; any other failure is carried to
    /// the caller's log.
    ///
    /// Break: `Ok(())` for every outcome, which counts a refusal that never
    /// happened and swallows a real accept error.
    #[test]
    fn reject_one_reports_the_accept_outcome() {
        let mut spare = SpareDescriptor::for_unix().expect("a unix reserve");

        let refused = spare.reject_one(|| Ok(())).expect("the reserve reopens");
        assert!(matches!(refused, RejectOutcome::Refused));
        assert!(spare.fd.is_some(), "the reserve is held again");

        for gone in [
            io::ErrorKind::WouldBlock,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::ConnectionReset,
        ] {
            let outcome = spare
                .reject_one(|| Err(io::Error::from(gone)))
                .expect("the reserve reopens");
            assert!(
                matches!(outcome, RejectOutcome::Nothing),
                "{gone:?} refused nothing"
            );
            assert!(spare.fd.is_some(), "the reserve is held again");
        }

        let failed = spare
            .reject_one(|| Err(io::Error::from(io::ErrorKind::PermissionDenied)))
            .expect("the reserve reopens");
        match failed {
            RejectOutcome::Failed(e) => {
                assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
            }
            other => panic!("a real accept error must reach the caller: {other:?}"),
        }
        assert!(spare.fd.is_some(), "the reserve is held again");
    }

    /// A send the socket stops part way reports the datagrams it took, and the
    /// retry sends only the rest: every datagram arrives once, in order.
    ///
    /// Break: a partial send reported as an error with no count, so the caller
    /// cannot know what is already on the wire and sends it again.
    #[test]
    fn a_blocked_send_reports_what_it_took() {
        const SEG: usize = 100;
        let receiver = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind receiver");
        receiver.set_nonblocking(true).expect("nonblocking");
        let sender = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind sender");
        sender.set_nonblocking(true).expect("nonblocking");
        let dest = receiver.local_addr().expect("receiver address");
        let batch = UdpBatch::without_gso();
        let payload: Vec<u8> = (0..500).map(|i| (i % 200) as u8).collect();

        test_send_blocked::fail_after_bytes(2 * SEG);
        let taken = batch
            .send(&sender, &payload, SEG, dest)
            .expect("the count a blocked send reports");
        assert_eq!(taken, 2 * SEG, "the datagrams the socket took");

        let rest = batch
            .send(&sender, &payload[taken..], SEG, dest)
            .expect("the rest");
        assert_eq!(rest, payload.len() - taken);

        let mut buf = [0u8; SEG];
        let mut got = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while got.len() < payload.len() && std::time::Instant::now() < deadline {
            match receiver.recv_from(&mut buf) {
                Ok((n, _)) => got.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // The datagrams cross loopback to the receiver's queue, so
                    // wait on the descriptor rather than assume the send has
                    // landed.
                    let _ = crate::wait_readable(
                        receiver.as_fd(),
                        std::time::Duration::from_millis(50),
                    );
                }
                Err(e) => panic!("receiver: {e}"),
            }
        }
        assert_eq!(got, payload, "every datagram once, in order");
    }

    /// A coalesced receive splits at its stride, and the last datagram may be
    /// shorter. A receive that was not coalesced stays one datagram, including
    /// the zero-length one UDP allows.
    ///
    /// This is the arithmetic every caller of `recv` now runs. It is checked on
    /// the host that has no GRO, so the Linux contract test that exercises the
    /// kernel's control message is not the only thing holding the split.
    ///
    /// Break: a split that drops the short last datagram, walks past `len`, or
    /// reports a zero-length receive as no datagram at all.
    #[test]
    fn a_coalesced_receive_splits_into_the_datagrams_that_were_sent() {
        let coalesced = RecvMeta {
            len: 337,
            stride: 100,
            ..RecvMeta::default()
        };
        assert_eq!(
            coalesced
                .datagrams()
                .map(|(offset, len)| {
                    assert!(offset + len <= coalesced.len);
                    len
                })
                .collect::<Vec<_>>(),
            [100, 100, 100, 37],
            "the run splits at its stride and the last datagram is short"
        );

        let single = RecvMeta {
            len: 1200,
            ..RecvMeta::default()
        };
        assert_eq!(
            single.datagrams().collect::<Vec<_>>(),
            [(0, 1200)],
            "a receive without a stride is one datagram"
        );

        let empty = RecvMeta::default();
        assert_eq!(
            empty.datagrams().collect::<Vec<_>>(),
            [(0, 0)],
            "a zero-length datagram is one datagram, not none"
        );
    }
}
