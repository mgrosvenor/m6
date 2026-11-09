//! Persistent non-blocking H2C (HTTP/2 cleartext) outbound client.
//!
//! A thin wrapper over the shared [`crate::h2_client`] core with the plain TCP
//! transport. One `H2cClientConn` is maintained per backend URL.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::mpsc;

use crate::forward::{HttpRequest, HttpResponse};
use crate::h2_client::{client_init_frame, H2Client, Pool, Transport};
use crate::poller::{Poller, Token};

/// The cleartext transport for `h2c://` backends.
pub struct H2cTransport {
    stream: TcpStream,
}

impl Transport for H2cTransport {
    fn connect(
        host: &str,
        port: u16,
        _tls: Option<std::sync::Arc<rustls::ClientConfig>>,
    ) -> io::Result<Self> {
        #[expect(clippy::disallowed_methods, reason = "blocking upstream connect; removed by EL-5b")]
        let mut stream = TcpStream::connect(format!("{}:{}", host, port))?;
        stream.set_nodelay(true)?;

        // Send client preface + SETTINGS while still in blocking mode so the
        // write never returns WouldBlock.
        stream.write_all(&client_init_frame())?;
        stream.flush()?;

        stream.set_nonblocking(true)?;
        Ok(Self { stream })
    }

    fn raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
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
    /// Connect to an H2C backend at `host:port`.
    pub fn connect(host: &str, port: u16) -> io::Result<Self> {
        Ok(Self(H2Client::connect(host, port, None)?))
    }
}

/// Persistent H2C connection pool, keyed by backend URL.
pub struct H2cClientPool(Pool<H2cTransport>);

impl Default for H2cClientPool {
    fn default() -> Self {
        Self::new()
    }
}

impl H2cClientPool {
    pub fn new() -> Self {
        Self(Pool::new())
    }

    /// Dispatch a request to the named backend URL.
    pub fn dispatch(
        &mut self,
        base_url: &str,
        req: &HttpRequest,
        client_ip: &str,
        original_host: &str,
    ) -> io::Result<mpsc::Receiver<io::Result<HttpResponse>>> {
        self.0
            .dispatch(base_url, req, client_ip, original_host, None)
    }

    /// Register new connections with the poller and drive all connections.
    pub fn drive_all(&mut self, poller: &Poller, token: Token) {
        self.0.drive_all(poller, token);
    }
}
