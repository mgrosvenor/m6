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

/// Send a QUIC CONNECTION_CLOSE and flush it before the connection is dropped.
///
/// The socket must be connected (`UdpSocket::connect`), which the bench client's
/// is: the close is addressed to the peer the socket already knows.
///
/// Dropping a connection without this leaves the peer holding it until its idle
/// timeout (30 s here). A client that reconnects every N requests therefore
/// piles up one idle connection per reconnect on the server, and the server
/// sweeps every live connection each event-loop turn. The bench client did
/// exactly that: about 950 idle connections in a 10 s run.
pub fn close_and_flush(conn: &mut quiche::Connection, udp: &UdpSocket) -> io::Result<()> {
    // A connection the peer already closed is not a failure to close.
    match conn.close(true, 0x100, b"") {
        Ok(()) | Err(quiche::Error::Done) => {}
        Err(e) => return Err(io::Error::other(e.to_string())),
    }
    let mut out = [0u8; 1350];
    loop {
        match conn.send(&mut out) {
            Ok((len, _info)) => {
                // Best effort: a close that cannot go out now changes nothing
                // about the client's next connection.
                match udp.send(&out[..len]) {
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e),
                }
            }
            Err(quiche::Error::Done) => break,
            Err(e) => return Err(io::Error::other(e.to_string())),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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

    /// A reconnect must put a CONNECTION_CLOSE on the wire, so the server can
    /// forget the connection at once instead of waiting out its idle timeout.
    /// The peer here is a real quiche server that has authenticated the
    /// client's Initial: it must see the close, which is what the test asserts
    /// from the other side.
    #[test]
    fn close_and_flush_sends_the_close() {
        let dir = tempfile::tempdir().unwrap();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();

        let server_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        server_sock
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let server_addr = server_sock.local_addr().unwrap();
        let client_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        client_sock.connect(server_addr).unwrap();
        client_sock
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let client_local = client_sock.local_addr().unwrap();

        let mut client_cfg = crate::probe::bench_quic_client_config(true).unwrap();
        let scid = quiche::ConnectionId::from_ref(&[7u8; 16]);
        let mut client = quiche::connect(
            Some("localhost"),
            &scid,
            client_local,
            server_addr,
            &mut client_cfg,
        )
        .unwrap();

        // The client's first flight, fed to a server that will then close.
        let mut out = [0u8; 1350];
        let (n, _) = client.send(&mut out).unwrap();
        client_sock.send(&out[..n]).unwrap();
        let mut buf = [0u8; 65535];
        let (n, from) = server_sock.recv_from(&mut buf).unwrap();

        let mut server_cfg = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
        server_cfg
            .load_cert_chain_from_pem_file(cert_path.to_str().unwrap())
            .unwrap();
        server_cfg
            .load_priv_key_from_pem_file(key_path.to_str().unwrap())
            .unwrap();
        server_cfg.set_application_protos(&[b"h3"]).unwrap();
        let server_scid = quiche::ConnectionId::from_ref(&[9u8; 16]);
        let mut server =
            quiche::accept(&server_scid, None, server_addr, from, &mut server_cfg).unwrap();
        // Authenticates the peer, which is what makes a close sendable.
        let _ = server.recv(
            &mut buf[..n],
            quiche::RecvInfo {
                from,
                to: server_addr,
            },
        );

        // The helper sends on a connected socket, which is what the bench
        // client has. Connect this test peer to the client it just heard from.
        server_sock.connect(client_local).unwrap();
        close_and_flush(&mut server, &server_sock).unwrap();

        let n = client_sock
            .recv(&mut buf)
            .expect("the CONNECTION_CLOSE reached the peer");
        assert!(n > 0);
        client
            .recv(
                &mut buf[..n],
                quiche::RecvInfo {
                    from: server_addr,
                    to: client_local,
                },
            )
            .ok();
        assert!(
            client.is_closed() || client.is_draining(),
            "the peer saw the connection closed"
        );
    }
}
