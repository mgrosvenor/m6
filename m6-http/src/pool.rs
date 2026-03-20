/// Backend pool management: socket discovery, least-connections load balancing.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::BackendConfig;
use rustls_native_certs;
use webpki_roots;

/// State of a single backend socket member.
#[derive(Debug)]
struct PoolMember {
    path: PathBuf,
    /// In-flight connection count.
    connections: usize,
    /// If temporarily removed due to failure, when to retry.
    retry_at: Option<Instant>,
    /// Number of consecutive failures (for backoff).
    failure_count: u32,
}

impl PoolMember {
    fn new(path: PathBuf) -> Self {
        PoolMember { path, connections: 0, retry_at: None, failure_count: 0 }
    }

    fn is_available(&self) -> bool {
        match self.retry_at {
            None => true,
            Some(t) => Instant::now() >= t,
        }
    }

    fn backoff_duration(&self) -> Duration {
        // After failure_count incremented: 1→1s, 2→2s, 3→4s, ... max 30s
        let secs = if self.failure_count == 0 {
            1
        } else {
            (1u64 << (self.failure_count - 1)).min(30)
        };
        Duration::from_secs(secs)
    }

    fn mark_failed(&mut self) {
        self.failure_count += 1;
        self.retry_at = Some(Instant::now() + self.backoff_duration());
    }

    fn mark_success(&mut self) {
        self.failure_count = 0;
        self.retry_at = None;
    }
}

/// A pool for a single named backend (socket-based).
pub struct BackendPool {
    pub name: String,
    pub socket_glob: String,
    members: Vec<PoolMember>,
}

impl BackendPool {
    pub fn new(name: String, socket_glob: String) -> Self {
        BackendPool { name, socket_glob, members: Vec::new() }
    }

    /// Add a socket path to the pool (if not already present).
    pub fn add_socket(&mut self, path: PathBuf) {
        if !self.members.iter().any(|m| m.path == path) {
            tracing::info!(pool = %self.name, socket = %path.display(), "pool: socket added");
            self.members.push(PoolMember::new(path));
        }
    }

    /// Remove a socket path from the pool.
    pub fn remove_socket(&mut self, path: &Path) {
        if let Some(pos) = self.members.iter().position(|m| m.path == path) {
            tracing::info!(pool = %self.name, socket = %path.display(), "pool: socket removed");
            self.members.remove(pos);
        }
    }

    /// Pick the least-connections available member.
    /// Returns index into members.
    fn pick_member(&mut self) -> Option<usize> {
        let now = Instant::now();
        let mut best: Option<usize> = None;
        let mut best_conns = usize::MAX;

        for (i, m) in self.members.iter().enumerate() {
            let available = match m.retry_at {
                None => true,
                Some(t) => now >= t,
            };
            if available && m.connections < best_conns {
                best_conns = m.connections;
                best = Some(i);
            }
        }
        best
    }

    /// Synchronous connect used by tests (not async).
    pub fn connect(&mut self) -> Result<(std::os::unix::net::UnixStream, usize), PoolError> {
        let idx = self.pick_member().ok_or(PoolError::Empty)?;
        let path = self.members[idx].path.clone();

        match std::os::unix::net::UnixStream::connect(&path) {
            Ok(stream) => {
                self.members[idx].mark_success();
                self.members[idx].connections += 1;
                Ok((stream, idx))
            }
            Err(e) => {
                tracing::warn!(
                    pool = %self.name,
                    socket = %path.display(),
                    error = %e,
                    "pool: connection failed, backing off"
                );
                self.members[idx].mark_failed();
                Err(PoolError::ConnectFailed(e))
            }
        }
    }

    /// Pick the socket path for the best available member (for async callers).
    /// Returns (socket_path, member_index). Caller must call release() when done.
    pub fn pick_socket(&mut self) -> Result<(PathBuf, usize), PoolError> {
        let idx = self.pick_member().ok_or(PoolError::Empty)?;
        let path = self.members[idx].path.clone();
        self.members[idx].connections += 1;
        Ok((path, idx))
    }

    /// Mark a connection attempt as failed (for async callers).
    pub fn mark_failed(&mut self, idx: usize) {
        if let Some(m) = self.members.get_mut(idx) {
            tracing::warn!(
                pool = %self.name,
                socket = %m.path.display(),
                "pool: connection failed, backing off"
            );
            m.connections = m.connections.saturating_sub(1);
            m.mark_failed();
        }
    }

    /// Decrement connection count for a member.
    pub fn release(&mut self, idx: usize) {
        if let Some(m) = self.members.get_mut(idx) {
            if m.connections > 0 {
                m.connections -= 1;
            }
        }
    }

    /// Check whether the given path matches this pool's socket glob.
    pub fn matches_glob(&self, path: &Path) -> bool {
        let path_str = path.to_string_lossy();
        match glob::Pattern::new(&self.socket_glob) {
            Ok(pattern) => pattern.matches(&path_str),
            Err(_) => false,
        }
    }

    /// Rescan the glob and sync member list.
    pub fn rescan(&mut self) {
        let found: Vec<PathBuf> = match glob::glob(&self.socket_glob) {
            Ok(paths) => paths.flatten().collect(),
            Err(_) => return,
        };

        // Remove members no longer present
        self.members.retain(|m| {
            if found.contains(&m.path) {
                true
            } else {
                tracing::info!(pool = %self.name, socket = %m.path.display(), "pool: socket gone (rescan)");
                false
            }
        });

        // Add new members
        for p in found {
            if !self.members.iter().any(|m| m.path == p) {
                tracing::info!(pool = %self.name, socket = %p.display(), "pool: socket found (rescan)");
                self.members.push(PoolMember::new(p));
            }
        }
    }

    /// Number of active (available) members.
    pub fn active_count(&self) -> usize {
        self.members.iter().filter(|m| m.is_available()).count()
    }

    /// Total member count (including temporarily unavailable).
    pub fn total_count(&self) -> usize {
        self.members.len()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("pool is empty")]
    Empty,
    #[error("connection failed: {0}")]
    ConnectFailed(std::io::Error),
}

/// A URL-based backend (remote HTTP/HTTPS upstream).
struct UrlBackend {
    name: String,
    url: String,
    /// TLS client config built once at startup (avoids per-request cert loading).
    tls_config: std::sync::Arc<rustls::ClientConfig>,
    skip_verify: bool,
}

fn build_tls_client_config_alpn(skip_verify: bool, alpn: &[u8]) -> rustls::ClientConfig {
    if skip_verify {
        // Danger: accept any certificate. Test/dev only.
        let mut cfg = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(std::sync::Arc::new(SkipVerifier))
            .with_no_client_auth();
        cfg.alpn_protocols = vec![alpn.to_vec()];
        cfg
    } else {
        let mut root_store = rustls::RootCertStore::empty();
        let native_roots = rustls_native_certs::load_native_certs();
        for cert in native_roots.certs {
            let _ = root_store.add(cert);
        }
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let mut cfg = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        cfg.alpn_protocols = vec![alpn.to_vec()];
        cfg
    }
}

fn build_tls_client_config(skip_verify: bool) -> rustls::ClientConfig {
    build_tls_client_config_alpn(skip_verify, b"http/1.1")
}

/// Build a TLS client config advertising HTTP/2 via ALPN.
/// Used for `h2s://` backends.
pub fn build_h2_tls_client_config(skip_verify: bool) -> rustls::ClientConfig {
    build_tls_client_config_alpn(skip_verify, b"h2")
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

impl PoolManager {
    pub fn new() -> Self {
        PoolManager { pools: Vec::new(), url_backends: Vec::new() }
    }

    pub fn from_config(backends: &[BackendConfig]) -> Self {
        let mut mgr = PoolManager::new();
        for b in backends {
            if let Some(ref sockets) = b.sockets {
                let mut pool = BackendPool::new(b.name.clone(), sockets.clone());
                pool.rescan();
                mgr.pools.push((b.name.clone(), pool));
            } else if let Some(ref url) = b.url {
                // h2s:// backends need ALPN "h2"; all others use "http/1.1".
                let tls_config = if url.starts_with("h2s://") {
                    std::sync::Arc::new(build_h2_tls_client_config(b.tls_skip_verify))
                } else {
                    std::sync::Arc::new(build_tls_client_config(b.tls_skip_verify))
                };
                mgr.url_backends.push(UrlBackend {
                    name: b.name.clone(),
                    url: url.clone(),
                    tls_config,
                    skip_verify: b.tls_skip_verify,
                });
            }
        }
        mgr
    }

    pub fn get_pool_mut(&mut self, name: &str) -> Option<&mut BackendPool> {
        self.pools.iter_mut().find(|(n, _)| n == name).map(|(_, p)| p)
    }

    pub fn get_pool(&self, name: &str) -> Option<&BackendPool> {
        self.pools.iter().find(|(n, _)| n == name).map(|(_, p)| p)
    }

    pub fn get_url(&self, name: &str) -> Option<&str> {
        self.url_backends.iter().find(|b| b.name == name).map(|b| b.url.as_str())
    }

    /// Returns (url, tls_config, skip_verify) for a URL backend.
    pub fn get_url_info(&self, name: &str) -> Option<(&str, std::sync::Arc<rustls::ClientConfig>, bool)> {
        self.url_backends.iter().find(|b| b.name == name).map(|b| {
            (b.url.as_str(), b.tls_config.clone(), b.skip_verify)
        })
    }

    /// Handle a socket appearing — add to matching pool.
    pub fn socket_appeared(&mut self, path: &Path) {
        for (_, pool) in self.pools.iter_mut() {
            if pool.matches_glob(path) {
                pool.add_socket(path.to_path_buf());
            }
        }
    }

    /// Handle a socket disappearing — remove from matching pool.
    pub fn socket_disappeared(&mut self, path: &Path) {
        for (_, pool) in self.pools.iter_mut() {
            pool.remove_socket(path);
        }
    }

    /// Rescan all pools.
    pub fn rescan_all(&mut self) {
        for (_, pool) in self.pools.iter_mut() {
            pool.rescan();
        }
    }

    /// Total active (non-failed) members across all pools — for stats reporting.
    pub fn total_active_members(&self) -> usize {
        self.pools.iter().map(|(_, p)| p.active_count()).sum()
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
    use tempfile::TempDir;

    #[test]
    fn test_empty_pool_returns_error() {
        let mut pool = BackendPool::new("test".to_string(), "/run/m6/*.sock".to_string());
        assert!(matches!(pool.connect(), Err(PoolError::Empty)));
    }

    #[test]
    fn test_add_remove_socket() {
        let dir = TempDir::new().unwrap();
        let sock_path = dir.path().join("test.sock");
        // Create a dummy file (not a real socket, but tests add/remove logic)
        std::fs::write(&sock_path, "").unwrap();

        let mut pool = BackendPool::new("test".to_string(), format!("{}/*.sock", dir.path().display()));
        pool.add_socket(sock_path.clone());
        assert_eq!(pool.total_count(), 1);

        pool.remove_socket(&sock_path);
        assert_eq!(pool.total_count(), 0);
    }

    #[test]
    fn test_socket_not_added_twice() {
        let path = PathBuf::from("/tmp/test.sock");
        let mut pool = BackendPool::new("test".to_string(), "/tmp/*.sock".to_string());
        pool.add_socket(path.clone());
        pool.add_socket(path.clone());
        assert_eq!(pool.total_count(), 1);
    }

    #[test]
    fn test_matches_glob() {
        let pool = BackendPool::new("test".to_string(), "/run/m6/m6-html-*.sock".to_string());
        assert!(pool.matches_glob(Path::new("/run/m6/m6-html-1.sock")));
        assert!(pool.matches_glob(Path::new("/run/m6/m6-html-worker.sock")));
        assert!(!pool.matches_glob(Path::new("/run/m6/other.sock")));
    }

    #[test]
    fn test_pool_manager_socket_appeared() {
        let mut mgr = PoolManager::new();
        mgr.add_pool(BackendPool::new(
            "m6-html".to_string(),
            "/run/m6/m6-html-*.sock".to_string(),
        ));

        let sock = Path::new("/run/m6/m6-html-1.sock");
        mgr.socket_appeared(sock);
        assert_eq!(mgr.get_pool("m6-html").unwrap().total_count(), 1);
    }

    #[test]
    fn test_pool_manager_socket_disappeared() {
        let mut mgr = PoolManager::new();
        let mut pool = BackendPool::new("m6-html".to_string(), "/run/m6/m6-html-*.sock".to_string());
        pool.add_socket(PathBuf::from("/run/m6/m6-html-1.sock"));
        mgr.add_pool(pool);

        mgr.socket_disappeared(Path::new("/run/m6/m6-html-1.sock"));
        assert_eq!(mgr.get_pool("m6-html").unwrap().total_count(), 0);
    }
}
