//! Persistent non-blocking H2C (HTTP/2 cleartext) outbound client.
//!
//! A thin wrapper over the shared [`crate::h2_client`] core with the plain TCP
//! transport. One `H2cClientConn` is maintained per backend URL.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::io::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

use crate::forward::{HttpRequest, PendingUrlContext, Waiter};
use crate::h2_client::{H2Client, H2Completion, Pool, Transport};
use m6_core::client::TlsClient;
use m6_core::platform::{Registry, Token};

/// The cleartext transport for `h2c://` backends.
pub struct H2cTransport {
    stream: TcpStream,
}

impl Transport for H2cTransport {
    fn connect(addr: SocketAddr, _tls: Option<&TlsClient>) -> io::Result<Self> {
        Ok(Self {
            stream: crate::h2_client::nonblocking_tcp_connect(addr)?,
        })
    }

    fn raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    fn take_error(&self) -> io::Result<Option<io::Error>> {
        self.stream.take_error()
    }

    fn is_connected(&self) -> bool {
        self.stream.peer_addr().is_ok()
    }

    fn is_handshaking(&self) -> bool {
        false
    }

    fn alpn_protocol(&self) -> Option<&[u8]> {
        None
    }

    fn scheme() -> &'static [u8] {
        b"http"
    }

    fn default_port() -> u16 {
        80
    }

    fn prefix() -> &'static str {
        "h2c"
    }

    fn flush_send(&mut self, send_buf: &mut Vec<u8>) -> Result<(), String> {
        let mut flushed = 0usize;
        while flushed < send_buf.len() {
            match self.stream.write(&send_buf[flushed..]) {
                Ok(0) => return Err(format!("{}: send got zero bytes", Self::prefix())),
                Ok(n) => flushed += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(format!("{}: send error: {}", Self::prefix(), e)),
            }
        }
        if flushed > 0 {
            send_buf.drain(..flushed);
        }
        Ok(())
    }

    fn fill_recv(&mut self, recv_buf: &mut Vec<u8>) -> Option<String> {
        let mut tmp = [0u8; 16384];
        loop {
            match self.stream.read(&mut tmp) {
                Ok(0) => return Some(format!("{}: connection closed", Self::prefix())),
                Ok(n) => recv_buf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Some(format!("{}: read error: {}", Self::prefix(), e)),
            }
        }
        None
    }

    fn wants_write(&self) -> bool {
        false
    }
}

/// Persistent non-blocking H2C connection.
pub struct H2cClientConn(H2Client<H2cTransport>);

impl std::ops::Deref for H2cClientConn {
    type Target = H2Client<H2cTransport>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for H2cClientConn {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl H2cClientConn {
    /// Connect to an H2C backend at `host:port` and wait for it to open.
    ///
    /// A test and benchmark helper: a service resolves the address at start and
    /// drives its pooled connection in the loop.
    pub fn connect(host: &str, port: u16) -> io::Result<Self> {
        let addr = m6_core::net::resolve(host, port)?;
        let mut client = H2Client::connect(addr, None)?;
        client.wait_until_open()?;
        Ok(Self(client))
    }
}

/// Persistent H2C connection pool, keyed by backend URL.
pub struct H2cClientPool(Pool<H2cTransport>);

impl H2cClientPool {
    /// `stream_timeout` is `backend_timeout_secs`: an h2 stream's deadline.
    pub fn new(stream_timeout: Duration) -> Self {
        Self(Pool::new(stream_timeout))
    }

    /// Ensure a connection exists for `base_url`. Returns whether one was made.
    pub fn ensure_connection(&mut self, base_url: &str, addr: SocketAddr) -> io::Result<bool> {
        self.0.ensure_connection(base_url, addr, None)
    }

    /// Time until the earliest stream deadline, capped at `tick`.
    pub fn timeout(&self, now: Instant, tick: Duration) -> Duration {
        self.0.timeout(now, tick)
    }

    /// Fail every stream past its deadline, cancelling it at the peer. Returns
    /// the completions and the tokens of connections left dead.
    pub fn expire(&mut self, now: Instant, registry: &Registry) -> (Vec<Token>, Vec<H2Completion>) {
        self.0.expire(now, registry)
    }

    /// The connection for `base_url`, to register and set its token.
    pub fn connection_mut(&mut self, base_url: &str) -> Option<&mut H2Client<H2cTransport>> {
        self.0.connection_mut(base_url)
    }

    /// Dispatch a request to the named backend URL.
    pub fn dispatch(
        &mut self,
        base_url: &str,
        req: &HttpRequest,
        client_ip: &str,
        original_host: &str,
        via_node: Option<&str>,
        registry: &Registry,
    ) -> io::Result<(Token, u32)> {
        self.0
            .dispatch(base_url, req, client_ip, original_host, via_node, registry)
    }

    /// Store who waits for an outbound stream's reply.
    pub fn set_waiter(
        &mut self,
        token: Token,
        stream_id: u32,
        waiter: Waiter,
        ctx: Box<PendingUrlContext>,
    ) {
        self.0.set_waiter(token, stream_id, waiter, ctx);
    }

    /// Drive the one connection whose token fired.
    pub fn drive_one(&mut self, token: Token, registry: &Registry) -> bool {
        self.0.drive_one(token, registry)
    }

    /// Take the completions the last drives produced.
    pub fn drain_completions(&mut self) -> Vec<H2Completion> {
        self.0.drain_completions()
    }

    /// Deregister and drop the connection under `token`, if any.
    pub fn remove_by_token(&mut self, token: Token, registry: &Registry) {
        self.0.remove_by_token(token, registry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A WINDOW_UPDATE frame: 4-byte increment on `stream_id`.
    fn window_update_frame(stream_id: u32, inc: u32) -> Vec<u8> {
        let mut f = Vec::with_capacity(13);
        f.extend_from_slice(&4u32.to_be_bytes()[1..]); // 3-byte length
        f.push(0x8); // WINDOW_UPDATE
        f.push(0); // no flags
        f.extend_from_slice(&stream_id.to_be_bytes());
        f.extend_from_slice(&inc.to_be_bytes());
        f
    }

    /// A request body larger than the connection send window is sent in two
    /// parts: the first window eagerly, the rest once WINDOW_UPDATE grants
    /// credit. The resumed DATA must be flushed in the same `drive` that reads
    /// the credit, or the connection waits for a writable edge that an
    /// edge-triggered poll never re-reports. The WINDOW_UPDATEs are injected
    /// straight into the receive buffer so the order is explicit and no sleep
    /// or socket timing can mask the flush.
    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "test fixture opens a loopback socketpair, not an upstream"
    )]
    fn flow_control_resume_flushes_in_the_same_drive() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(addr).unwrap();
        let (_peer, _) = listener.accept().unwrap();
        client.set_nonblocking(true).unwrap();

        let mut conn = H2Client::new(H2cTransport { stream: client });
        let req = HttpRequest {
            method: "POST".to_string(),
            path: "/".to_string(),
            query: None,
            version: "HTTP/1.1".to_string(),
            headers: vec![("content-length".to_string(), "65536".to_string())],
            body: vec![b'x'; 65_536],
        };
        let _rx = conn
            .dispatch(&req, "127.0.0.1", "127.0.0.1", "127.0.0.1", None)
            .unwrap();

        // First drive flushes HEADERS + the first connection window of DATA,
        // leaving one byte pending on flow control.
        conn.drive();
        assert_eq!(
            conn.send_buf_len_for_test(),
            0,
            "first window must flush in the first drive"
        );

        // Grant connection-level and stream-level credit back, exactly as the
        // origin does after consuming the first window.
        conn.inject_recv_for_test(&window_update_frame(0, 65_535));
        conn.inject_recv_for_test(&window_update_frame(1, 65_535));

        // Second drive reads the WINDOW_UPDATEs and pumps the final DATA byte.
        conn.drive();
        assert_eq!(
            conn.send_buf_len_for_test(),
            0,
            "resumed DATA must be flushed in the same drive, not left in send_buf"
        );
    }
}
