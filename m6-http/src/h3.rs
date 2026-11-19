//! HTTP/3 plumbing shared by the service and the bench client.
//!
//! Two facts the receive paths used to re-learn per datagram live here: a
//! socket's local address, and how to close a QUIC connection so the peer stops
//! holding it.

use std::io;
use std::net::{SocketAddr, UdpSocket};

/// The local address of a bound UDP socket, read once.
///
/// `UdpSocket::local_addr` is `getsockname(2)`. A socket's local address cannot
/// change while it is bound, and the HTTP/3 receive loop called it for every
/// received datagram: 398 syscalls for one 485 KB response, 43 for a 52 KB
/// page. The address is read when the socket is created and the stored value is
/// used for every `RecvInfo` afterwards.
///
/// It is the local endpoint of the QUIC path, which quiche needs for path
/// validation; it is not what chooses an `Iface`, which is classified by the
/// peer address from `recv_from`.
#[derive(Clone, Copy, Debug)]
pub struct LocalAddr(SocketAddr);

impl LocalAddr {
    /// Read the socket's local address. One `getsockname` per socket.
    pub fn read(socket: &UdpSocket) -> io::Result<Self> {
        #[cfg(test)]
        LOCAL_ADDR_READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(Self(socket.local_addr()?))
    }

    /// The stored address. No system call.
    #[inline]
    pub fn get(self) -> SocketAddr {
        self.0
    }
}

#[cfg(test)]
static LOCAL_ADDR_READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
fn local_addr_reads() -> usize {
    LOCAL_ADDR_READS.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The receive loop's contract: the address is read from the kernel once
    /// and every later read is the stored value.
    #[test]
    fn local_addr_is_read_once() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let before = local_addr_reads();
        let cached = LocalAddr::read(&socket).unwrap();
        assert_eq!(local_addr_reads(), before + 1, "one getsockname per socket");
        let expected = socket.local_addr().unwrap();
        assert_eq!(cached.get(), expected);
        for _ in 0..100 {
            assert_eq!(cached.get(), expected);
        }
        assert_eq!(
            local_addr_reads(),
            before + 1,
            "a stored read must not go back to the kernel"
        );
    }
}
