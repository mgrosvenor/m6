//! Persistent non-blocking H2S (HTTP/2 over TLS) outbound client.
//!
//! A thin wrapper over the shared [`crate::h2_client`] core with the rustls
//! transport. One `H2sTlsClientConn` is maintained per backend URL.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::ClientConnection;

use crate::forward::{HttpRequest, PendingUrlContext, Waiter};
use crate::h2_client::{H2Client, H2Completion, Pool, Transport};
use m6_core::client::TlsClient;
use m6_core::platform::{Registry, Token};

/// The TLS transport for `h2s://` backends.
pub struct H2sTransport {
    tcp: TcpStream,
    tls_conn: ClientConnection,
}

impl Transport for H2sTransport {
    fn connect(addr: SocketAddr, tls: Option<&TlsClient>) -> io::Result<Self> {
        let tls = tls.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{}: missing TLS config", Self::prefix()),
            )
        })?;
        let tcp = crate::h2_client::nonblocking_tcp_connect(addr)?;
        let tls_conn = ClientConnection::new(tls.config.clone(), tls.name.clone())
            .map_err(|e| io::Error::other(format!("{}: TLS init: {}", Self::prefix(), e)))?;
        Ok(Self { tcp, tls_conn })
    }

    fn take_error(&self) -> io::Result<Option<io::Error>> {
        self.tcp.take_error()
    }

    fn is_connected(&self) -> bool {
        self.tcp.peer_addr().is_ok()
    }

    fn is_handshaking(&self) -> bool {
        self.tls_conn.is_handshaking()
    }

    fn alpn_protocol(&self) -> Option<&[u8]> {
        self.tls_conn.alpn_protocol()
    }

    fn raw_fd(&self) -> RawFd {
        self.tcp.as_raw_fd()
    }

    fn scheme() -> &'static [u8] {
        b"https"
    }

    fn default_port() -> u16 {
        443
    }

    fn prefix() -> &'static str {
        "h2s"
    }

    fn flush_send(&mut self, send_buf: &mut Vec<u8>) -> Result<(), String> {
        // A short write here is normal, not an error: rustls caps its outgoing
        // plaintext buffer (64 KiB by default), so its `Writer` accepts as much
        // as fits and returns 0 once the buffer is full. Keep whatever was not
        // accepted and let the flush below drain the buffer into the socket;
        // the next `drive()` continues from there.
        if !send_buf.is_empty() {
            match self.tls_conn.writer().write(send_buf) {
                // Buffer full. Not an error: the flush below makes room.
                Ok(0) => {}
                Ok(n) => {
                    send_buf.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(format!("{}: TLS writer error: {}", Self::prefix(), e)),
            }
        }

        // ── Flush encrypted TLS output to TCP ────────────────────────────────
        loop {
            if !self.tls_conn.wants_write() {
                break;
            }
            match self.tls_conn.write_tls(&mut self.tcp) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(format!("{}: TLS send error: {}", Self::prefix(), e)),
            }
        }
        Ok(())
    }

    fn fill_recv(&mut self, recv_buf: &mut Vec<u8>) -> Option<String> {
        let mut tmp = [0u8; 16384];

        // Drain any plaintext already buffered in rustls (e.g. server SETTINGS
        // that arrived during the handshake).
        loop {
            match self.tls_conn.reader().read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => recv_buf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }

        loop {
            match self.tls_conn.read_tls(&mut self.tcp) {
                Ok(0) => return Some(format!("{}: connection closed", Self::prefix())),
                Ok(_) => match self.tls_conn.process_new_packets() {
                    Ok(_) => loop {
                        match self.tls_conn.reader().read(&mut tmp) {
                            Ok(0) => break,
                            Ok(n) => recv_buf.extend_from_slice(&tmp[..n]),
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                            Err(_) => break,
                        }
                    },
                    Err(e) => return Some(format!("{}: TLS process error: {}", Self::prefix(), e)),
                },
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Some(format!("{}: read error: {}", Self::prefix(), e)),
            }
        }
        None
    }

    fn wants_write(&self) -> bool {
        self.tls_conn.wants_write()
    }
}

/// Persistent non-blocking H2S (TLS) connection.
pub struct H2sTlsClientConn(H2Client<H2sTransport>);

impl std::ops::Deref for H2sTlsClientConn {
    type Target = H2Client<H2sTransport>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for H2sTlsClientConn {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl H2sTlsClientConn {
    /// Connect to an H2S backend at `host:port` and wait for it to open.
    ///
    /// A test and benchmark helper: a service resolves the address at start and
    /// drives its pooled connection in the loop.
    pub fn connect(
        host: &str,
        port: u16,
        tls_config: Arc<rustls::ClientConfig>,
    ) -> io::Result<Self> {
        let addr = m6_core::net::resolve(host, port)?;
        let name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{}: invalid server name: {host}", H2sTransport::prefix()),
            )
        })?;
        let tls = TlsClient {
            config: tls_config,
            name,
        };
        let mut client = H2Client::connect(addr, Some(&tls))?;
        client.wait_until_open()?;
        Ok(Self(client))
    }
}

/// Persistent H2S connection pool, keyed by backend URL.
pub struct H2sTlsClientPool(Pool<H2sTransport>);

impl H2sTlsClientPool {
    /// `stream_timeout` is `backend_timeout_secs`: an h2 stream's deadline.
    pub fn new(stream_timeout: Duration) -> Self {
        Self(Pool::new(stream_timeout))
    }

    /// Ensure a connection exists for `base_url`. Returns whether one was made.
    pub fn ensure_connection(
        &mut self,
        base_url: &str,
        addr: SocketAddr,
        tls: &TlsClient,
    ) -> io::Result<bool> {
        self.0.ensure_connection(base_url, addr, Some(tls))
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
    pub fn connection_mut(&mut self, base_url: &str) -> Option<&mut H2Client<H2sTransport>> {
        self.0.connection_mut(base_url)
    }

    /// Dispatch a request to the named backend URL.
    pub fn dispatch(
        &mut self,
        base_url: &str,
        req: &HttpRequest,
        client_ip: &str,
        original_host: &str,
        registry: &Registry,
    ) -> io::Result<(Token, u32)> {
        self.0
            .dispatch(base_url, req, client_ip, original_host, registry)
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
