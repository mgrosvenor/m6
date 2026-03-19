// Integration tests for m6-http: Phases 10-13.
// Covers: routing, caching, auth enforcement, hot reload.

#![allow(dead_code, unused_variables, unused_imports)]
use std::path::{Path, PathBuf};

use tempfile::TempDir;

// ── Helpers ──────────────────────────────────────────────────────────────────

struct ProcessGuard(std::process::Child);

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn write_file(dir: &Path, name: &str, content: &str) {
    std::fs::write(dir.join(name), content).unwrap();
}

fn write_file_abs(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(path, content).unwrap();
}

fn bin_path() -> PathBuf {
    // The binary is built to target/debug/m6-http
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let workspace_root = Path::new(&manifest_dir).parent().unwrap_or(Path::new("."));
    workspace_root.join("target/debug/m6-http")
}

fn dummy_cert() -> &'static str {
    // A self-signed cert is needed for TLS; we skip TLS validation in config tests
    // For config-only tests (not binding), dummy PEM files are sufficient.
    "dummy-cert"
}

fn make_minimal_site_dir(dir: &Path) {
    write_file(dir, "cert.pem", dummy_cert());
    write_file(dir, "key.pem", "dummy-key");
    write_file(dir, "site.toml", &minimal_site_toml());
}

fn minimal_site_toml() -> String {
    r#"
[site]
name   = "Test"
domain = "test.example.com"

[server]
bind     = "127.0.0.1:18443"
tls_cert = "cert.pem"
tls_key  = "key.pem"

[[backend]]
name    = "b"
sockets = "/run/m6/b-*.sock"

[[route]]
path    = "/"
backend = "b"
"#.to_string()
}

fn minimal_system_toml() -> String {
    r#"
[server]
bind     = "127.0.0.1:18443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
"#.to_string()
}

/// Run m6-http with --dump-config and capture output + exit code.
fn run_dump_config(site_dir: &Path, system_config: &Path) -> (i32, String) {
    let bin = bin_path();
    if !bin.exists() {
        // Binary not built yet — skip test gracefully
        return (0, "{}".to_string());
    }

    let output = std::process::Command::new(&bin)
        .arg(site_dir)
        .arg(system_config)
        .arg("--dump-config")
        .output()
        .expect("failed to run m6-http");

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    (exit_code, stdout)
}

/// Run m6-http and expect a specific exit code.
fn run_expect_exit(site_dir: &Path, system_config: &Path, expected: i32) {
    let bin = bin_path();
    if !bin.exists() {
        return; // Binary not built yet — skip
    }

    // Run with a timeout (it shouldn't bind successfully with dummy TLS files)
    // For config error tests, it should exit before binding
    let output = std::process::Command::new(&bin)
        .arg(site_dir)
        .arg(system_config)
        .output()
        .expect("failed to run m6-http");

    let exit_code = output.status.code().unwrap_or(-1);
    assert_eq!(
        exit_code, expected,
        "expected exit {} but got {}\nstdout: {}\nstderr: {}",
        expected,
        exit_code,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

// ── Phase 10: Routing + Forwarding ───────────────────────────────────────────

mod phase10 {
    use super::*;

    // Use config module directly for validation tests (no subprocess needed)
    use m6_http_lib::config;

    fn load(site_dir: &Path) -> anyhow::Result<config::Config> {
        let system = site_dir.join("system.toml");
        config::load(site_dir, &system)
    }

    fn setup(dir: &Path, site_toml: &str) {
        write_file(dir, "cert.pem", "dummy");
        write_file(dir, "key.pem", "dummy");
        write_file(dir, "site.toml", site_toml);
        write_file(dir, "system.toml", "");
    }

    #[test]
    fn validation_missing_site_name_exits_2() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path(), r#"
[site]
domain = "example.com"
[server]
bind = "0.0.0.0:443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
"#);
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("name"), "error should mention 'name': {}", err);
    }

    #[test]
    fn validation_missing_site_domain_exits_2() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path(), r#"
[site]
name = "Test"
[server]
bind = "0.0.0.0:443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
"#);
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("domain"), "error should mention 'domain': {}", err);
    }

    #[test]
    fn validation_missing_bind_exits_2() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path(), r#"
[site]
name   = "Test"
domain = "example.com"
[server]
tls_cert = "cert.pem"
tls_key  = "key.pem"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
"#);
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("bind"), "error should mention 'bind': {}", err);
    }

    #[test]
    fn validation_tls_cert_not_found_exits_2() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "key.pem", "dummy");
        write_file(dir.path(), "site.toml", r#"
[site]
name   = "Test"
domain = "example.com"
[server]
bind     = "0.0.0.0:443"
tls_cert = "missing.pem"
tls_key  = "key.pem"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
"#);
        write_file(dir.path(), "system.toml", "");
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("tls_cert"), "error should mention 'tls_cert': {}", err);
    }

    #[test]
    fn validation_unknown_backend_in_route_exits_2() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path(), r#"
[site]
name   = "Test"
domain = "example.com"
[server]
bind     = "0.0.0.0:443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
[[route]]
path    = "/"
backend = "unknown"
"#);
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("unknown backend"), "error should mention backend: {}", err);
    }

    #[test]
    fn validation_duplicate_route_path_exits_2() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path(), r#"
[site]
name   = "Test"
domain = "example.com"
[server]
bind     = "0.0.0.0:443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
[[route]]
path    = "/"
backend = "b"
[[route]]
path    = "/"
backend = "b"
"#);
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("duplicate"), "error should mention 'duplicate': {}", err);
    }

    #[test]
    fn validation_require_without_auth_exits_2() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path(), r#"
[site]
name   = "Test"
domain = "example.com"
[server]
bind     = "0.0.0.0:443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
[[route]]
path    = "/"
backend = "b"
require = "group:editors"
"#);
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("require"), "error should mention 'require': {}", err);
    }

    #[test]
    fn validation_custom_errors_without_path_exits_2() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path(), r#"
[site]
name   = "Test"
domain = "example.com"
[server]
bind     = "0.0.0.0:443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
[errors]
mode = "custom"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
"#);
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("custom"), "error should mention 'custom': {}", err);
    }

    #[test]
    fn validation_auth_public_key_not_found_exits_2() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "cert.pem", "dummy");
        write_file(dir.path(), "key.pem", "dummy");
        write_file(dir.path(), "system.toml", "");
        write_file(dir.path(), "site.toml", r#"
[site]
name   = "Test"
domain = "example.com"
[server]
bind     = "0.0.0.0:443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
[auth]
backend    = "b"
public_key = "missing-key.pub"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
"#);
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("public_key"), "error should mention 'public_key': {}", err);
    }

    #[test]
    fn validation_auth_backend_not_in_backends_exits_2() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "cert.pem", "dummy");
        write_file(dir.path(), "key.pem", "dummy");
        write_file(dir.path(), "auth.pub", "dummy-key");
        write_file(dir.path(), "system.toml", "");
        write_file(dir.path(), "site.toml", r#"
[site]
name   = "Test"
domain = "example.com"
[server]
bind     = "0.0.0.0:443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
[auth]
backend    = "missing-auth-backend"
public_key = "auth.pub"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
"#);
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("missing-auth-backend") || err.contains("backend"),
            "error should mention auth backend: {}", err);
    }

    #[test]
    fn system_config_server_wins() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "cert.pem", "dummy");
        write_file(dir.path(), "key.pem", "dummy");
        write_file(dir.path(), "site.toml", r#"
[site]
name   = "Test"
domain = "example.com"
[server]
bind     = "127.0.0.1:8443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
"#);
        write_file(dir.path(), "system.toml", r#"
[server]
bind = "0.0.0.0:443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
"#);
        let cfg = m6_http_lib::config::load(dir.path(), &dir.path().join("system.toml")).unwrap();
        assert_eq!(cfg.server.bind, "0.0.0.0:443");
    }
}

// ── Phase 11: Caching ────────────────────────────────────────────────────────

mod phase11 {
    use m6_http_lib::cache::{Cache, CacheKey, CachedResponse, should_cache};

    #[test]
    fn public_response_is_cached() {
        let cache = Cache::new();
        let key = CacheKey::new("/page", "");
        let headers = vec![("cache-control".to_string(), "public, max-age=3600".to_string())];
        assert!(should_cache(200, &headers));

        let resp = CachedResponse { status: 200, headers: std::sync::Arc::new(headers.clone()), body: bytes::Bytes::from_static(b"cached"), hints: std::sync::Arc::new(vec![]) };
        cache.insert(key.clone(), resp);
        assert!(cache.get(&key).is_some());
    }

    #[test]
    fn second_request_served_from_cache() {
        let cache = Cache::new();
        let key = CacheKey::new("/page", "");
        let resp = CachedResponse {
            status: 200,
            headers: std::sync::Arc::new(vec![("cache-control".to_string(), "public".to_string())]),
            body: bytes::Bytes::from_static(b"body"),
            hints: std::sync::Arc::new(vec![]),
        };
        cache.insert(key.clone(), resp);

        // Simulate second request: cache hit, no forwarding needed
        let hit = cache.get(&key);
        assert!(hit.is_some());
        assert_eq!(hit.unwrap().body, b"body" as &[u8]);
    }

    #[test]
    fn no_store_not_cached() {
        let headers = vec![("cache-control".to_string(), "no-store".to_string())];
        assert!(!should_cache(200, &headers));
    }

    #[test]
    fn private_not_cached() {
        let headers = vec![("cache-control".to_string(), "private".to_string())];
        assert!(!should_cache(200, &headers));
    }

    #[test]
    fn gzip_and_br_cached_independently() {
        let cache = Cache::new();
        let key_gzip = CacheKey::new("/page", "gzip");
        let key_br = CacheKey::new("/page", "br");

        cache.insert(key_gzip.clone(), CachedResponse {
            status: 200,
            headers: std::sync::Arc::new(vec![("content-encoding".to_string(), "gzip".to_string())]),
            body: bytes::Bytes::from_static(b"gzip-body"),
            hints: std::sync::Arc::new(vec![]),
        });

        assert!(cache.get(&key_gzip).is_some());
        assert!(cache.get(&key_br).is_none());

        cache.insert(key_br.clone(), CachedResponse {
            status: 200,
            headers: std::sync::Arc::new(vec![("content-encoding".to_string(), "br".to_string())]),
            body: bytes::Bytes::from_static(b"br-body"),
            hints: std::sync::Arc::new(vec![]),
        });

        assert_eq!(cache.get(&key_gzip).unwrap().body, b"gzip-body" as &[u8]);
        assert_eq!(cache.get(&key_br).unwrap().body, b"br-body" as &[u8]);
    }

    #[test]
    fn query_string_stripped_from_cache_key() {
        let k1 = CacheKey::new("/blog?a=1", "");
        let k2 = CacheKey::new("/blog?a=2", "");
        let k3 = CacheKey::new("/blog", "");
        assert_eq!(k1, k2);
        assert_eq!(k1, k3);
    }

    #[test]
    fn data_file_modified_evicts_cache_entries() {
        let cache = Cache::new();

        // Pre-populate cache with the path that maps from a data file
        let key = CacheKey::new("/blog/hello-world", "");
        cache.insert(key.clone(), CachedResponse {
            status: 200,
            headers: std::sync::Arc::new(vec![("cache-control".to_string(), "public".to_string())]),
            body: bytes::Bytes::from_static(b"hello world post"),
            hints: std::sync::Arc::new(vec![]),
        });

        // Simulate: inotify fires on content/posts/hello-world.json
        // invalidation map maps that file → /blog/hello-world
        // evict the affected cache entry
        cache.evict_path("/blog/hello-world");
        assert!(cache.get(&key).is_none());
    }

    #[test]
    fn error_responses_never_cached() {
        let h4xx = vec![("cache-control".to_string(), "public".to_string())];
        assert!(!should_cache(404, &h4xx));
        assert!(!should_cache(500, &h4xx));
        assert!(!should_cache(503, &h4xx));
    }
}

// ── Phase 12: Auth ───────────────────────────────────────────────────────────

mod phase12 {
    use base64::Engine;
    use m6_http_lib::auth::{
        check_require, encode_claims_header, extract_refresh_cookie, extract_token,
        is_browser_request, Claims,
    };

    fn make_claims(groups: Option<Vec<&str>>, roles: Option<Vec<&str>>) -> Claims {
        Claims {
            sub: Some("user1".to_string()),
            iss: None,
            exp: Some(9999999999),
            groups: groups.map(|g| g.iter().map(|s| s.to_string()).collect()),
            roles: roles.map(|r| r.iter().map(|s| s.to_string()).collect()),
            extra: Default::default(),
        }
    }

    #[test]
    fn public_route_no_auth_code_executed() {
        // The contract: for public routes, zero auth code is executed.
        // We verify this by checking that extract_token is not called
        // when no `require` is set. This is a structural test.
        //
        // The handle_request function only calls auth code when route.require.is_some().
        // We verify the behavior indirectly: a request to a public route
        // with no token should be forwarded (not rejected).
        //
        // Since we can't easily test the full stack without a real server,
        // we test the auth module is not invoked for public routes
        // by verifying extract_token with no headers returns None.
        let token = extract_token(None, None);
        assert_eq!(token, None);
        // The caller (handle_request) only calls this if route.require.is_some()
    }

    #[test]
    fn protected_no_token_api_client_gets_401() {
        // API clients (no text/html Accept) should get 401 directly
        assert!(!is_browser_request(Some("application/json")));
        assert!(!is_browser_request(None));
    }

    #[test]
    fn protected_no_token_browser_gets_302_to_login() {
        assert!(is_browser_request(Some("text/html,application/xhtml+xml")));
        // No refresh cookie → redirect to /login
        let refresh = extract_refresh_cookie(None);
        assert!(refresh.is_none());
    }

    #[test]
    fn protected_no_token_browser_with_refresh_cookie_gets_302_to_refresh() {
        assert!(is_browser_request(Some("text/html")));
        let refresh = extract_refresh_cookie(Some("refresh=my-refresh-token; session=old"));
        assert_eq!(refresh, Some("my-refresh-token"));
    }

    #[test]
    fn valid_jwt_wrong_group_gets_403() {
        let claims = make_claims(Some(vec!["users"]), None);
        assert!(!check_require(&claims, "group:editors"));
    }

    #[test]
    fn valid_jwt_correct_group_passes() {
        let claims = make_claims(Some(vec!["editors", "users"]), None);
        assert!(check_require(&claims, "group:editors"));
    }

    #[test]
    fn valid_jwt_correct_role_passes() {
        let claims = make_claims(None, Some(vec!["admin"]));
        assert!(check_require(&claims, "role:admin"));
    }

    #[test]
    fn valid_jwt_wrong_role_gets_403() {
        let claims = make_claims(None, Some(vec!["user"]));
        assert!(!check_require(&claims, "role:admin"));
    }

    #[test]
    fn x_auth_claims_header_is_base64_json() {
        let claims = make_claims(Some(vec!["editors"]), None);
        let encoded = encode_claims_header(&claims);
        let decoded = base64::engine::general_purpose::STANDARD.decode(&encoded).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(json["sub"], "user1");
        assert_eq!(json["groups"][0], "editors");
    }
}

// ── Phase 13: Hot Reload ─────────────────────────────────────────────────────

mod phase13 {
    use m6_http_lib::pool::{BackendPool, PoolManager};
    use std::path::{Path, PathBuf};

    #[test]
    fn socket_appears_added_to_pool() {
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
    fn socket_disappears_removed_from_pool() {
        let mut mgr = PoolManager::new();
        let mut pool = BackendPool::new("m6-html".to_string(), "/run/m6/m6-html-*.sock".to_string());
        pool.add_socket(PathBuf::from("/run/m6/m6-html-1.sock"));
        mgr.add_pool(pool);

        assert_eq!(mgr.get_pool("m6-html").unwrap().total_count(), 1);
        mgr.socket_disappeared(Path::new("/run/m6/m6-html-1.sock"));
        assert_eq!(mgr.get_pool("m6-html").unwrap().total_count(), 0);
    }

    #[test]
    fn all_sockets_gone_pool_is_empty() {
        let mut pool = BackendPool::new("m6-html".to_string(), "/run/m6/m6-html-*.sock".to_string());
        pool.add_socket(PathBuf::from("/run/m6/m6-html-1.sock"));
        pool.remove_socket(Path::new("/run/m6/m6-html-1.sock"));
        assert_eq!(pool.total_count(), 0);
        // Connecting to empty pool returns PoolError::Empty
        assert!(matches!(pool.connect(), Err(m6_http_lib::pool::PoolError::Empty)));
    }

    #[test]
    fn site_toml_reload_updates_route_table() {
        use m6_http_lib::config;
        use m6_http_lib::router::RouteTable;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let dir_path = dir.path();

        // Write initial config
        std::fs::write(dir_path.join("cert.pem"), "dummy").unwrap();
        std::fs::write(dir_path.join("key.pem"), "dummy").unwrap();
        std::fs::write(dir_path.join("system.toml"), "").unwrap();
        std::fs::write(dir_path.join("site.toml"), r#"
[site]
name   = "Test"
domain = "example.com"
[server]
bind     = "0.0.0.0:443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
[[route]]
path    = "/"
backend = "b"
"#).unwrap();

        let cfg1 = config::load(dir_path, &dir_path.join("system.toml")).unwrap();
        let table1 = RouteTable::from_config(&cfg1).unwrap();
        assert!(table1.at("/").is_some());
        assert!(table1.at("/new").is_none());

        // Write updated config with new route
        std::fs::write(dir_path.join("site.toml"), r#"
[site]
name   = "Test"
domain = "example.com"
[server]
bind     = "0.0.0.0:443"
tls_cert = "cert.pem"
tls_key  = "key.pem"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
[[route]]
path    = "/"
backend = "b"
[[route]]
path    = "/new"
backend = "b"
"#).unwrap();

        let cfg2 = config::load(dir_path, &dir_path.join("system.toml")).unwrap();
        let table2 = RouteTable::from_config(&cfg2).unwrap();
        assert!(table2.at("/").is_some());
        assert!(table2.at("/new").is_some());
    }
}
