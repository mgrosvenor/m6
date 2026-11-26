//! Publishing: one backend reload, then one `PURGE` to m6-http.
//!
//! A publish writes files, and m6-http answers readers from its cache until a
//! purge evicts the changed paths. The publish is two steps, in this order:
//!
//! 1. The publisher reloads the backend that renders the changed content by
//!    sending `POST /genid` on that backend's own Unix socket, m6-core's
//!    reserved reload signal. The backend re-reads its files and keeps the old
//!    site if the reload fails.
//! 2. The publisher sends `PURGE /*` to m6-http, which evicts every path from
//!    the origin and every registered cache. The request needs a token with the
//!    write claim (`[registration] write_require` in the system config).
//!
//! The `PURGE` runs on an `Upstream`, the non-blocking client slot m6-core
//! supplies, so a slow m6-http holds one client's answer and never the loop.
//! The backend reload is a local Unix-socket exchange, so it runs inline.

use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use m6_core::client::{self, Addr, TlsClient, Upstream};
use m6_core::rustls::pki_types::ServerName;
use m6_core::rustls::{ClientConfig, RootCertStore};
use serde_json::{Map, Value};

/// Where the `PURGE` goes, what it carries, and which backend to reload first.
/// Built once at start.
pub struct Publisher {
    addr: SocketAddr,
    tls: TlsClient,
    /// The `Host` header value.
    host: String,
    /// The bearer token that carries the write claim.
    token: String,
    /// The backend socket the reload signal goes to. `None` for a publish that
    /// only flushes the cache and changes no content.
    reload: Option<PathBuf>,
    timeout: Duration,
}

impl Publisher {
    /// Read the `[publish]` table of the service's configuration:
    ///
    /// - `url`: the origin's URL, `https://<host>[:port]`
    /// - `connect`: optional `ip:port` to connect to instead of resolving the
    ///   URL's host
    /// - `token_file`: absolute path of a file holding a token with the write
    ///   claim. A missing or empty file is an error, so the service stops at
    ///   start and says why
    /// - `ca_file`: optional absolute path of a PEM certificate to trust. A
    ///   development node signs its own certificate. Without it the system's
    ///   certificate store is trusted
    /// - `timeout_ms`: the longest a publish may take, default 10000
    /// - `reload`: optional backend Unix socket, which receives `POST /genid`
    ///   to re-read its files before the purge. Absent for a publish that only
    ///   flushes the cache
    ///
    /// Start only: it reads files and may resolve a name.
    pub fn load(config: &Map<String, Value>) -> Result<Publisher, String> {
        let table = config
            .get("publish")
            .and_then(Value::as_object)
            .ok_or("[publish] is missing: a publish needs the origin's URL")?;
        let text = |key: &str| table.get(key).and_then(Value::as_str);
        let url = text("url").ok_or("[publish] url is missing")?;
        let rest = url
            .strip_prefix("https://")
            .ok_or_else(|| format!("[publish] url {url:?} must start with https://"))?;
        let authority = match rest.find('/') {
            Some(i) => &rest[..i],
            None => rest,
        };
        let (name, port) = match authority.rsplit_once(':') {
            Some((name, port)) => (
                name,
                port.parse::<u16>()
                    .map_err(|e| format!("[publish] url port {port:?}: {e}"))?,
            ),
            None => (authority, 443),
        };
        let addr = match text("connect") {
            Some(connect) => connect
                .parse::<SocketAddr>()
                .map_err(|e| format!("[publish] connect {connect:?}: {e}"))?,
            None => m6_core::net::resolve(name, port).map_err(|e| format!("{name}: {e}"))?,
        };

        let token_file = text("token_file").ok_or("[publish] token_file is missing")?;
        let token = std::fs::read_to_string(token_file)
            .map_err(|e| format!("[publish] token_file {token_file}: {e}"))?
            .trim()
            .to_string();
        if token.is_empty() {
            return Err(format!("[publish] token_file {token_file} is empty"));
        }

        let reload = match text("reload") {
            Some(path) => {
                let reload = PathBuf::from(path);
                if !reload.is_absolute() {
                    return Err(format!(
                        "[publish] reload {reload:?} must be an absolute path"
                    ));
                }
                Some(reload)
            }
            None => None,
        };

        let roots = match text("ca_file") {
            Some(ca_file) => {
                let pem = std::fs::read(ca_file)
                    .map_err(|e| format!("[publish] ca_file {ca_file}: {e}"))?;
                let mut roots = RootCertStore::empty();
                for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
                    let cert = cert.map_err(|e| format!("[publish] ca_file {ca_file}: {e}"))?;
                    roots
                        .add(cert)
                        .map_err(|e| format!("[publish] ca_file {ca_file}: {e}"))?;
                }
                roots
            }
            None => client::system_roots().map_err(|e| e.to_string())?,
        };
        // The provider is named, because a build that links two of them has no
        // process default for `ClientConfig::builder()` to pick.
        let provider = Arc::new(m6_core::rustls::crypto::aws_lc_rs::default_provider());
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let tls = TlsClient {
            config: Arc::new(config),
            name: ServerName::try_from(name.to_string())
                .map_err(|e| format!("[publish] url host {name:?}: {e}"))?,
        };
        let timeout = Duration::from_millis(
            table
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(10_000),
        );
        Ok(Publisher {
            addr,
            tls,
            host: authority.to_string(),
            token,
            reload,
            timeout,
        })
    }

    /// The longest a publish may take, for the loop's deadline table.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Reload the backend that renders the changed content. `POST /genid` is
    /// m6-core's reserved reload signal: the backend runs its start path again
    /// and swaps in the new site, keeping the old one on a failed load.
    ///
    /// This is a local Unix-socket exchange, so it runs inline on the loop. A
    /// publisher with no `reload` socket is a cache-only flush and succeeds at
    /// once.
    pub fn reload_backend(&self) -> Result<(), String> {
        let Some(reload) = self.reload.as_ref() else {
            return Ok(());
        };
        let mut stream =
            UnixStream::connect(reload).map_err(|e| format!("{}: {e}", reload.display()))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| e.to_string())?;
        stream
            .write_all(
                b"POST /genid HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .map_err(|e| e.to_string())?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).map_err(|e| e.to_string())?;
        let status = std::str::from_utf8(&raw)
            .ok()
            .and_then(|text| text.split("\r\n").next())
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(format!("backend reload answered {status}"))
        }
    }

    /// Write the `PURGE /*` request into `up` and begin the non-blocking
    /// connect to m6-http.
    pub fn start(&self, up: &mut Upstream, now: Instant) -> io::Result<()> {
        write!(
            up.request_buf(),
            "PURGE /* HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            self.host, self.token
        )?;
        up.start(
            &Addr::Tcp(self.addr),
            Some(&self.tls),
            false,
            now + self.timeout,
        )
    }
}

/// What the finished `PURGE` says: `Ok` for a 200, else the status m6-http
/// answered with.
pub fn outcome(up: &Upstream) -> Result<(), String> {
    let mut slots = client::reply_slots();
    match up.reply(&mut slots) {
        Some(reply) if reply.status == 200 => Ok(()),
        Some(reply) => Err(format!("m6-http answered PURGE with {}", reply.status)),
        None => Err("m6-http's answer to PURGE could not be read".to_string()),
    }
}
