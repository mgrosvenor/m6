//! Backend pool management: socket discovery and round-robin member choice.
use std::path::{Path, PathBuf};

use crate::config::BackendConfig;

/// A pool for a single named backend (socket-based).
pub struct BackendPool {
    pub name: String,
    pub socket_glob: String,
    /// The member sockets, in the order the glob found them.
    members: Vec<PathBuf>,
    /// The member the next request takes.
    next: usize,
}

impl BackendPool {
    pub fn new(name: String, socket_glob: String) -> Self {
        BackendPool {
            name,
            socket_glob,
            members: Vec::new(),
            next: 0,
        }
    }

    /// Add a socket path to the pool (if not already present).
    pub fn add_socket(&mut self, path: PathBuf) {
        if !self.members.iter().any(|m| m == &path) {
            m6_core::log::info!(pool = %self.name, socket = %path.display(), "pool: socket added");
            self.members.push(path);
        }
    }

    /// The next member's socket path, in turn, or `None` when the pool is
    /// empty.
    ///
    /// Each call advances the cursor. A request whose connect fails takes the
    /// member after the one it just tried, so one dead member costs one failed
    /// connect and the request is still served by the next member.
    pub fn take_member(&mut self) -> Option<&Path> {
        let path = self.members.get(self.next)?;
        self.next = (self.next + 1) % self.members.len();
        Some(path)
    }

    /// Expand the glob once at start and add every matching socket.
    fn expand_glob(&mut self) {
        let found: Vec<PathBuf> = match glob::glob(&self.socket_glob) {
            Ok(paths) => paths.flatten().collect(),
            Err(_) => return,
        };

        for p in found {
            if !self.members.iter().any(|m| m == &p) {
                m6_core::log::info!(pool = %self.name, socket = %p.display(), "pool: socket found");
                self.members.push(p);
            }
        }
    }

    /// Total member count.
    pub fn total_count(&self) -> usize {
        self.members.len()
    }
}

/// A URL-based backend (remote HTTP/HTTPS upstream).
pub struct UrlBackend {
    pub name: String,
    /// The base URL, kept as the h2 pools' key.
    pub url: String,
    /// The authority's host, without the port: the `Host` a forwarded request
    /// names and the name a TLS connection verifies. Resolved at start.
    pub host: String,
    pub scheme: UrlScheme,
    /// The host's address, resolved once at [`PoolManager::resolve`]. A change
    /// of address needs a restart.
    addr: Option<std::net::SocketAddr>,
    /// The TLS client config and server name for a TLS scheme, built at start.
    pub tls: Option<m6_core::client::TlsClient>,
    /// Keys of this backend's slots whose connection is open and registered
    /// for read, so a peer close is seen and the key removed before a request
    /// reuses it. A fixed array made at start.
    pub idle: IdleSlots,
    /// Skip TLS certificate verification. Test and development only.
    pub skip_verify: bool,
}

impl UrlBackend {
    /// The address resolved at start. The loop never starts before it is set.
    pub fn addr(&self) -> std::net::SocketAddr {
        self.addr.expect("a URL backend is resolved at start")
    }
}

/// Which protocol a URL backend speaks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UrlScheme {
    Http,
    Https,
    H2c,
    H2s,
}

impl UrlScheme {
    /// The scheme named by `url`, or `None` for one m6 does not serve.
    fn of(url: &str) -> Option<UrlScheme> {
        if url.starts_with("http://") {
            Some(UrlScheme::Http)
        } else if url.starts_with("https://") {
            Some(UrlScheme::Https)
        } else if url.starts_with("h2c://") {
            Some(UrlScheme::H2c)
        } else if url.starts_with("h2s://") {
            Some(UrlScheme::H2s)
        } else {
            None
        }
    }

    /// The prefix `parse_host_port` takes, and the scheme's default port.
    fn prefix_and_port(self) -> (&'static str, u16) {
        match self {
            UrlScheme::Http => ("http", 80),
            UrlScheme::Https => ("https", 443),
            UrlScheme::H2c => ("h2c", 80),
            UrlScheme::H2s => ("h2s", 443),
        }
    }
}

/// The most idle connections one URL backend keeps open for reuse.
const MAX_IDLE_URL_CONNECTIONS: usize = 8;

/// Keys of open idle connections to one URL backend, last in first out.
///
/// A key stays only while its connection is open and its descriptor is watched
/// for a close. `remove` is how a close is taken out of the list.
pub struct IdleSlots {
    keys: [usize; MAX_IDLE_URL_CONNECTIONS],
    len: usize,
}

impl IdleSlots {
    fn new() -> IdleSlots {
        IdleSlots {
            keys: [0; MAX_IDLE_URL_CONNECTIONS],
            len: 0,
        }
    }

    /// A key whose connection is open, or `None` when none is idle.
    pub fn take(&mut self) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        Some(self.keys[self.len])
    }

    /// Store `key` for reuse. False when the list is full, so the caller closes
    /// the connection and frees the slot instead.
    pub fn push(&mut self, key: usize) -> bool {
        if self.len == self.keys.len() {
            return false;
        }
        self.keys[self.len] = key;
        self.len += 1;
        true
    }

    /// Take `key` out, because its connection is gone.
    pub fn remove(&mut self, key: usize) {
        if let Some(pos) = self.keys[..self.len].iter().position(|k| *k == key) {
            self.keys.copy_within(pos + 1..self.len, pos);
            self.len -= 1;
        }
    }
}

fn build_tls_client_config_alpn(
    skip_verify: bool,
    alpn: &[u8],
) -> anyhow::Result<rustls::ClientConfig> {
    if skip_verify {
        // Danger: accept any certificate. Test/dev only.
        let mut cfg = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(std::sync::Arc::new(SkipVerifier))
            .with_no_client_auth();
        cfg.alpn_protocols = vec![alpn.to_vec()];
        Ok(cfg)
    } else {
        let mut root_store = rustls::RootCertStore::empty();
        let native_roots = rustls_native_certs::load_native_certs();
        let first_error = native_roots
            .errors
            .first()
            .map(|e| format!(": {e}"))
            .unwrap_or_default();
        for cert in native_roots.certs {
            let _ = root_store.add(cert);
        }
        if root_store.is_empty() {
            anyhow::bail!(
                "no certificate authority in the system store (load_native_certs found none{first_error}): install the host's CA certificates, or set tls_skip_verify on this backend"
            );
        }
        let mut cfg = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        cfg.alpn_protocols = vec![alpn.to_vec()];
        Ok(cfg)
    }
}

fn build_tls_client_config(skip_verify: bool) -> anyhow::Result<rustls::ClientConfig> {
    build_tls_client_config_alpn(skip_verify, b"http/1.1")
}

/// Build a TLS client config advertising HTTP/2 via ALPN.
/// Used for `h2s://` backends.
pub fn build_h2_tls_client_config(skip_verify: bool) -> anyhow::Result<rustls::ClientConfig> {
    build_tls_client_config_alpn(skip_verify, b"h2")
}

/// The config and server name a TLS URL backend authenticates with, built once
/// at start beside its resolved address.
fn tls_client(
    host: &str,
    config: std::sync::Arc<rustls::ClientConfig>,
) -> anyhow::Result<m6_core::client::TlsClient> {
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| anyhow::anyhow!("invalid server name {host:?}: {e}"))?;
    Ok(m6_core::client::TlsClient { config, name })
}

/// A no-op TLS certificate verifier for testing with self-signed certs.
#[derive(Debug)]
struct SkipVerifier;

impl rustls::client::danger::ServerCertVerifier for SkipVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Manager for all backend pools.
pub struct PoolManager {
    /// (name, pool) — linear scan; 2–5 entries in practice.
    pools: Vec<(String, BackendPool)>,
    /// URL backends — TLS config built once at startup.
    url_backends: Vec<UrlBackend>,
}

impl Default for PoolManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PoolManager {
    pub fn new() -> Self {
        PoolManager {
            pools: Vec::new(),
            url_backends: Vec::new(),
        }
    }

    pub fn from_config(backends: &[BackendConfig]) -> anyhow::Result<Self> {
        let mut mgr = PoolManager::new();
        for b in backends {
            if let Some(ref sockets) = b.sockets {
                let mut pool = BackendPool::new(b.name.clone(), sockets.clone());
                pool.expand_glob();
                if pool.members.is_empty() {
                    anyhow::bail!(
                        "[[backend]] {name}: sockets = {glob:?} matches no socket. Start the backends before m6-http (systemd After=)",
                        name = b.name,
                        glob = sockets
                    );
                }
                mgr.pools.push((b.name.clone(), pool));
            } else if let Some(ref url) = b.url {
                let scheme = UrlScheme::of(url).ok_or_else(|| {
                    anyhow::anyhow!(
                        "[[backend]] {name}: url {url:?} must start with http://, https://, h2c:// or h2s://",
                        name = b.name
                    )
                })?;
                mgr.url_backends.push(UrlBackend {
                    name: b.name.clone(),
                    url: url.clone(),
                    host: String::new(),
                    scheme,
                    addr: None,
                    tls: None,
                    idle: IdleSlots::new(),
                    skip_verify: b.tls_skip_verify,
                });
            }
        }
        Ok(mgr)
    }

    /// Resolve every URL backend's host once, and build its TLS client. Start
    /// only: this blocks in the system resolver, and a name that does not
    /// resolve stops the service with the backend and host named.
    pub fn resolve(&mut self) -> anyhow::Result<()> {
        for b in &mut self.url_backends {
            let (prefix, default_port) = b.scheme.prefix_and_port();
            let (host, port) = crate::h2_client::parse_host_port(&b.url, prefix, default_port)
                .map_err(|e| anyhow::anyhow!("[[backend]] {}: {e}", b.name))?;
            let addr = m6_core::net::resolve(&host, port).map_err(|e| {
                anyhow::anyhow!("[[backend]] {}: {host} does not resolve: {e}", b.name)
            })?;
            b.addr = Some(addr);
            let tls_config = match b.scheme {
                UrlScheme::Https => Some(build_tls_client_config(b.skip_verify)),
                UrlScheme::H2s => Some(build_h2_tls_client_config(b.skip_verify)),
                UrlScheme::Http | UrlScheme::H2c => None,
            };
            b.tls = match tls_config {
                Some(config) => {
                    let config =
                        config.map_err(|e| anyhow::anyhow!("[[backend]] {}: {e}", b.name))?;
                    Some(tls_client(&host, std::sync::Arc::new(config))?)
                }
                None => None,
            };
            b.host = host;
        }
        Ok(())
    }

    pub fn get_pool_mut(&mut self, name: &str) -> Option<&mut BackendPool> {
        self.pools
            .iter_mut()
            .find(|(n, _)| n == name)
            .map(|(_, p)| p)
    }

    pub fn get_pool(&self, name: &str) -> Option<&BackendPool> {
        self.pools.iter().find(|(n, _)| n == name).map(|(_, p)| p)
    }

    pub fn get_url(&self, name: &str) -> Option<&str> {
        self.url_backends
            .iter()
            .find(|b| b.name == name)
            .map(|b| b.url.as_str())
    }

    /// The URL backend named `name`, or `None` when the name is a socket pool.
    pub fn find_url(&self, name: &str) -> Option<usize> {
        self.url_backends.iter().position(|b| b.name == name)
    }

    pub fn url_backend(&self, idx: usize) -> &UrlBackend {
        &self.url_backends[idx]
    }

    pub fn url_backend_mut(&mut self, idx: usize) -> &mut UrlBackend {
        &mut self.url_backends[idx]
    }

    /// Every socket member of every pool, in pool order and then in the order
    /// the glob found them. A change sequence sends one `POST /genid` to each
    /// of these on its own socket, which is the one place the pool's member
    /// choice is deliberately bypassed.
    pub fn socket_members(&self) -> Vec<PathBuf> {
        self.pools
            .iter()
            .flat_map(|(_, pool)| pool.members.iter().cloned())
            .collect()
    }

    /// Total members across all socket pools, for stats reporting.
    pub fn total_members(&self) -> usize {
        self.pools.iter().map(|(_, p)| p.total_count()).sum()
    }

    /// Per-pool member counts for `/perf`.
    ///
    /// This is the count alone: the pool carries no per-member health state,
    /// so every member stays in rotation. A member whose backend is down is
    /// found when a request connects to it, and that request takes the next
    /// member.
    pub fn pool_health(&self) -> Vec<(String, usize)> {
        self.pools
            .iter()
            .map(|(name, p)| (name.clone(), p.total_count()))
            .collect()
    }

    /// Names of URL backends (a cache node's origin, typically).
    ///
    /// Presence only. Proving one reachable means a network round trip, and a
    /// health check that makes a network call is a health check that can hang
    /// on exactly the outage it is meant to report.
    pub fn url_backend_names(&self) -> Vec<String> {
        self.url_backends.iter().map(|b| b.name.clone()).collect()
    }

    /// Add a pool directly (used in tests).
    pub fn add_pool(&mut self, pool: BackendPool) {
        let name = pool.name.clone();
        self.pools.push((name, pool));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_member_returns_each_member_in_turn() {
        let mut pool = BackendPool::new("app".to_string(), "/tmp/*.sock".to_string());
        let a = PathBuf::from("/tmp/app-a.sock");
        let b = PathBuf::from("/tmp/app-b.sock");
        pool.add_socket(a.clone());
        pool.add_socket(b.clone());
        let mut order = Vec::new();
        for _ in 0..4 {
            order.push(pool.take_member().map(Path::to_path_buf));
        }
        assert_eq!(
            order,
            vec![Some(a.clone()), Some(b.clone()), Some(a), Some(b)],
            "the turn must repeat every member in order"
        );
    }

    #[test]
    fn an_empty_pool_has_no_member() {
        let mut pool = BackendPool::new("test".to_string(), "/run/m6/*.sock".to_string());
        assert!(pool.take_member().is_none());
    }

    #[test]
    fn a_socket_is_not_added_twice() {
        let path = PathBuf::from("/tmp/test.sock");
        let mut pool = BackendPool::new("test".to_string(), "/tmp/*.sock".to_string());
        pool.add_socket(path.clone());
        pool.add_socket(path);
        assert_eq!(pool.total_count(), 1);
    }
}
