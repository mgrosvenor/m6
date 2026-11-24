//! Publishing: one `POST /genid` to m6-http, driven by the service's own loop.
//!
//! A publish writes files, and m6-http answers readers from its cache until
//! the site's generation advances. `POST /genid` on the source node advances
//! it: the source reloads every backend on a Unix socket, flushes its cache and
//! answers with the new ID. The request needs a token that carries the write
//! claim (`[genid] write_require` in the system config).
//!
//! The request runs on an `Upstream`, the non-blocking client slot m6-core
//! supplies, so a slow m6-http holds one client's answer and never the loop.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use m6_core::client::{self, Addr, TlsClient, Upstream};
use m6_core::rustls::pki_types::ServerName;
use m6_core::rustls::{ClientConfig, RootCertStore};
use serde_json::{Map, Value};

/// Where the `POST /genid` goes and what it carries. Built once at start.
pub struct Publisher {
    addr: SocketAddr,
    tls: TlsClient,
    /// The `Host` header value.
    host: String,
    /// The request target: the URL's path.
    target: String,
    token: String,
    timeout: Duration,
}

impl Publisher {
    /// Read the `[genid]` table of the service's configuration:
    ///
    /// - `url`: the publish URL, `https://<host>[:port]/genid`
    /// - `connect`: optional `ip:port` to connect to instead of resolving the
    ///   URL's host
    /// - `token_file`: absolute path of a file holding a token with the write
    ///   claim. A missing or empty file is an error, so the service stops at
    ///   start and says why
    /// - `ca_file`: optional absolute path of a PEM certificate to trust. A
    ///   development node signs its own certificate. Without it the system's
    ///   certificate store is trusted
    /// - `timeout_ms`: the longest a publish may take, default 10000
    ///
    /// Start only: it reads files and may resolve a name.
    pub fn load(config: &Map<String, Value>) -> Result<Publisher, String> {
        let table = config
            .get("genid")
            .and_then(Value::as_object)
            .ok_or("[genid] is missing: a publish needs the source node's /genid URL")?;
        let text = |key: &str| table.get(key).and_then(Value::as_str);
        let url = text("url").ok_or("[genid] url is missing")?;
        let rest = url
            .strip_prefix("https://")
            .ok_or_else(|| format!("[genid] url {url:?} must start with https://"))?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (name, port) = match authority.rsplit_once(':') {
            Some((name, port)) => (
                name,
                port.parse::<u16>()
                    .map_err(|e| format!("[genid] url port {port:?}: {e}"))?,
            ),
            None => (authority, 443),
        };
        let addr = match text("connect") {
            Some(connect) => connect
                .parse::<SocketAddr>()
                .map_err(|e| format!("[genid] connect {connect:?}: {e}"))?,
            None => m6_core::net::resolve(name, port).map_err(|e| format!("{name}: {e}"))?,
        };

        let token_file = text("token_file").ok_or("[genid] token_file is missing")?;
        let token = std::fs::read_to_string(token_file)
            .map_err(|e| format!("[genid] token_file {token_file}: {e}"))?
            .trim()
            .to_string();
        if token.is_empty() {
            return Err(format!("[genid] token_file {token_file} is empty"));
        }

        let roots = match text("ca_file") {
            Some(ca_file) => {
                let pem = std::fs::read(ca_file)
                    .map_err(|e| format!("[genid] ca_file {ca_file}: {e}"))?;
                let mut roots = RootCertStore::empty();
                for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
                    let cert = cert.map_err(|e| format!("[genid] ca_file {ca_file}: {e}"))?;
                    roots
                        .add(cert)
                        .map_err(|e| format!("[genid] ca_file {ca_file}: {e}"))?;
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
                .map_err(|e| format!("[genid] url host {name:?}: {e}"))?,
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
            target: path.to_string(),
            token,
            timeout,
        })
    }

    /// The longest a publish may take, for the loop's deadline table.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Write the request into `up` and begin the non-blocking connect.
    pub fn start(&self, up: &mut Upstream, now: Instant) -> io::Result<()> {
        use std::io::Write;
        write!(
            up.request_buf(),
            "POST {} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            self.target, self.host, self.token
        )?;
        up.start(
            &Addr::Tcp(self.addr),
            Some(&self.tls),
            false,
            now + self.timeout,
        )
    }
}

/// What the finished exchange says: `Ok` for a 200, else the status m6-http
/// answered with.
pub fn outcome(up: &Upstream) -> Result<(), String> {
    let mut slots = client::reply_slots();
    match up.reply(&mut slots) {
        Some(reply) if reply.status == 200 => Ok(()),
        Some(reply) => Err(format!(
            "m6-http answered POST /genid with {}",
            reply.status
        )),
        None => Err("m6-http's answer to POST /genid could not be read".to_string()),
    }
}
