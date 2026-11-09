//! Persistent non-blocking H2S (HTTP/2 over TLS) outbound client.
//!
//! A thin wrapper over the shared [`crate::h2_client`] core with the rustls
//! transport. One `H2sTlsClientConn` is maintained per backend URL.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::mpsc;
use std::sync::Arc;

use rustls::ClientConnection;

use crate::forward::{HttpRequest, HttpResponse};
use crate::h2_client::{client_init_frame, H2Client, Pool, Transport};
use crate::poller::{Poller, Token};

/// The TLS transport for `h2s://` backends.
pub struct H2sTransport {
    tcp: TcpStream,
    tls_conn: ClientConnection,
}

impl Transport for H2sTransport {
    fn connect(host: &str, port: u16, tls: Option<Arc<rustls::ClientConfig>>) -> io::Result<Self> {
        let tls_config = tls.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{}: missing TLS config", Self::prefix()),
            )
        })?;

        #[expect(clippy::disallowed_methods, reason = "blocking upstream connect; removed by EL-5b")]
        let tcp = TcpStream::connect(format!("{}:{}", host, port))?;
        tcp.set_nodelay(true)?;

        let server_name =
            rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{}: invalid server name: {}", Self::prefix(), host),
                )
            })?;

        let conn = ClientConnection::new(tls_config, server_name)
            .map_err(|e| io::Error::other(format!("{}: TLS init: {}", Self::prefix(), e)))?;

        // Use StreamOwned for the blocking setup phase (TLS handshake + initial frames).
        let mut tls_stream = rustls::StreamOwned::new(conn, tcp);

        // This write triggers the TLS handshake.
        tls_stream.write_all(&client_init_frame())?;
        tls_stream.flush()?;

        // Verify ALPN negotiation.
        if tls_stream.conn.alpn_protocol() != Some(b"h2".as_slice()) {
            let got = tls_stream
                .conn
                .alpn_protocol()
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .unwrap_or_else(|| "none".to_string());
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{}: server did not negotiate h2 (got: {})",
                    Self::prefix(),
                    got
                ),
            ));
        }

        let (mut tls_conn, tcp) = tls_stream.into_parts();
        tcp.set_nonblocking(true)?;

        // Drain any plaintext that arrived during the TLS handshake (e.g. the
        // server's SETTINGS frame). It stays in the rustls buffer and will be
        // picked up by the first drive() call.
        let _ = tls_conn.process_new_packets();

        Ok(Self { tcp, tls_conn })
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
    /// Connect to an H2S backend at `host:port`.
    pub fn connect(
        host: &str,
        port: u16,
        tls_config: Arc<rustls::ClientConfig>,
    ) -> io::Result<Self> {
        Ok(Self(H2Client::connect(host, port, Some(tls_config))?))
    }
}

/// Persistent H2S connection pool, keyed by backend URL.
pub struct H2sTlsClientPool(Pool<H2sTransport>);

impl Default for H2sTlsClientPool {
    fn default() -> Self {
        Self::new()
    }
}

impl H2sTlsClientPool {
    pub fn new() -> Self {
        Self(Pool::new())
    }

    /// Dispatch a request to the named backend URL.
    ///
    /// `tls_config` must have ALPN `h2` — use `pool::build_h2_tls_client_config()`.
    pub fn dispatch(
        &mut self,
        base_url: &str,
        req: &HttpRequest,
        client_ip: &str,
        original_host: &str,
        tls_config: Arc<rustls::ClientConfig>,
    ) -> io::Result<mpsc::Receiver<io::Result<HttpResponse>>> {
        self.0
            .dispatch(base_url, req, client_ip, original_host, Some(tls_config))
    }

    /// Register new connections with the poller and drive all connections.
    pub fn drive_all(&mut self, poller: &Poller, token: Token) {
        self.0.drive_all(poller, token);
    }
}
