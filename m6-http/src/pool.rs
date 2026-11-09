/// Backend pool management: socket discovery, least-connections load balancing.
use std::path::PathBuf;
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
        PoolMember {
            path,
            connections: 0,
            retry_at: None,
            failure_count: 0,
        }
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
            // The exponent is capped before the shift: 2^5 already passes the
            // 30 s ceiling, and an uncapped shift overflows at the 65th
            // failure, which a release build turns into an abort.
            (1u64 << (self.failure_count - 1).min(5)).min(30)
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
        BackendPool {
            name,
            socket_glob,
            members: Vec::new(),
        }
    }

    /// Add a socket path to the pool (if not already present).
    pub fn add_socket(&mut self, path: PathBuf) {
        if !self.members.iter().any(|m| m.path == path) {
            tracing::info!(pool = %self.name, socket = %path.display(), "pool: socket added");
            self.members.push(PoolMember::new(path));
        }
    }

    /// Pick the least-connections available member.
    /// Returns index into members.
    fn pick_member(&mut self) -> Option<usize> {
        let mut best: Option<usize> = None;
        let mut best_conns = usize::MAX;

        for (i, m) in self.members.iter().enumerate() {
            if m.is_available() && m.connections < best_conns {
                best_conns = m.connections;
                best = Some(i);
            }
        }
        best
    }

    /// Synchronous connect used by tests (not async).
    #[expect(clippy::disallowed_methods, reason = "test-only after this job; removed by EL-1")]
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

    /// A request to this member completed: release its connection and clear
    /// its failures. The count is transport failures in a row (connect, I/O,
    /// timeout), so any completed exchange clears it, whatever the status: a
    /// member that answers 503 is reachable.
    pub fn release(&mut self, idx: usize) {
        if let Some(m) = self.members.get_mut(idx) {
            if m.connections > 0 {
                m.connections -= 1;
            }
            m.mark_success();
        }
    }

    /// Release a connection that never reached the member, because m6-http
    /// refused the request itself. Says nothing about the member's health.
    pub fn cancel(&mut self, idx: usize) {
        if let Some(m) = self.members.get_mut(idx) {
            m.connections = m.connections.saturating_sub(1);
        }
    }

    /// Expand the glob once at start and add every matching socket.
    fn expand_glob(&mut self) {
        let found: Vec<PathBuf> = match glob::glob(&self.socket_glob) {
            Ok(paths) => paths.flatten().collect(),
            Err(_) => return,
        };

        for p in found {
            if !self.members.iter().any(|m| m.path == p) {
                tracing::info!(pool = %self.name, socket = %p.display(), "pool: socket found");
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
        Ok(mgr)
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

    /// Returns (url, tls_config, skip_verify) for a URL backend.
    pub fn get_url_info(
        &self,
        name: &str,
    ) -> Option<(&str, std::sync::Arc<rustls::ClientConfig>, bool)> {
        self.url_backends
            .iter()
            .find(|b| b.name == name)
            .map(|b| (b.url.as_str(), b.tls_config.clone(), b.skip_verify))
    }

    /// Total active (non-failed) members across all pools — for stats reporting.
    pub fn total_active_members(&self) -> usize {
        self.pools.iter().map(|(_, p)| p.active_count()).sum()
    }

    /// Per-pool occupancy for the health endpoint.
    ///
    /// Deliberately per-pool rather than the summed `total_active_members()`
    /// above: a node with a healthy m6-html and a dead render-contact sums to
    /// a non-zero total and looks fine, when one of its backends cannot serve
    /// at all. The sum answers "is anything alive"; health needs "is
    /// everything alive".
    pub fn pool_health(&self) -> Vec<(String, usize, usize)> {
        self.pools
            .iter()
            .map(|(name, p)| (name.clone(), p.active_count(), p.total_count()))
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
    fn a_long_run_of_failures_backs_off_thirty_seconds_and_never_overflows() {
        // An uncapped shift overflows at the 65th failure; a release build
        // aborts on it.
        let mut m = PoolMember::new(PathBuf::from("/tmp/x.sock"));
        for _ in 0..1000 {
            m.mark_failed();
        }
        assert_eq!(m.backoff_duration(), Duration::from_secs(30));
    }

    #[test]
    fn a_successful_request_clears_the_member_failures() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("a.sock");
        let mut pool =
            BackendPool::new("p".into(), dir.path().join("*.sock").display().to_string());
        pool.add_socket(sock);
        let (_, idx) = pool.pick_socket().expect("member");
        pool.mark_failed(idx);
        assert_eq!(pool.members[idx].failure_count, 1);
        pool.members[idx].retry_at = None;
        let (_, idx) = pool.pick_socket().expect("member again");
        pool.release(idx);
        assert_eq!(
            pool.members[idx].failure_count, 0,
            "a success must reset the count"
        );
        assert!(pool.members[idx].retry_at.is_none());
    }

    #[test]
    fn a_request_refused_before_forwarding_leaves_the_member_alone() {
        // The wrong outcome: m6-http's own refusal backs the member off, so
        // one malformed request makes a one-member pool answer 502.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pool =
            BackendPool::new("p".into(), dir.path().join("*.sock").display().to_string());
        pool.add_socket(dir.path().join("a.sock"));
        let (_, idx) = pool.pick_socket().expect("member");
        pool.cancel(idx);
        assert_eq!(pool.members[idx].connections, 0);
        assert_eq!(pool.members[idx].failure_count, 0);
        assert!(pool.members[idx].retry_at.is_none());
    }

    #[test]
    fn test_empty_pool_returns_error() {
        let mut pool = BackendPool::new("test".to_string(), "/run/m6/*.sock".to_string());
        assert!(matches!(pool.connect(), Err(PoolError::Empty)));
    }

    #[test]
    fn test_socket_not_added_twice() {
        let path = PathBuf::from("/tmp/test.sock");
        let mut pool = BackendPool::new("test".to_string(), "/tmp/*.sock".to_string());
        pool.add_socket(path.clone());
        pool.add_socket(path.clone());
        assert_eq!(pool.total_count(), 1);
    }

    /// A failed member is skipped while its backoff lasts.
    ///
    /// The bug this catches: `pick_member` ignoring the backoff and keeping a
    /// dead socket in rotation, so a request still lands on the failed member
    /// and answers 502 instead of shifting to a healthy one.
    #[test]
    fn test_pick_member_skips_failed_member_during_backoff() {
        let mut pool = BackendPool::new("app".to_string(), "/tmp/*.sock".to_string());
        pool.add_socket(PathBuf::from("/tmp/app-a.sock"));
        pool.add_socket(PathBuf::from("/tmp/app-b.sock"));

        pool.mark_failed(0);

        assert_eq!(
            pool.pick_member(),
            Some(1),
            "the failed member must be skipped until its backoff ends"
        );
    }

    /// A member is picked again once its backoff has ended.
    ///
    /// The bug this catches: `pick_member` treating the backoff as permanent,
    /// so a recovered backend stays out of the pool for good. The backoff is
    /// rolled forward in place rather than slept through, because a fixed sleep
    /// is a timing guess.
    #[test]
    fn test_pick_member_picks_member_again_after_backoff_ends() {
        let mut pool = BackendPool::new("app".to_string(), "/tmp/*.sock".to_string());
        pool.add_socket(PathBuf::from("/tmp/app-a.sock"));
        pool.add_socket(PathBuf::from("/tmp/app-b.sock"));

        pool.mark_failed(0);
        pool.members[0].retry_at = Some(Instant::now() - Duration::from_secs(1));

        assert_eq!(
            pool.pick_member(),
            Some(0),
            "the recovered member must be picked again after its backoff ends"
        );
    }
}
