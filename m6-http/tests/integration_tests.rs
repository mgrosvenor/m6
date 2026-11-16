// Integration tests for m6-http: Phases 10-13.
// Covers: routing, caching, auth enforcement, config loading.

use std::path::Path;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn write_file(dir: &Path, name: &str, content: &str) {
    let replaced = content.replace("{DIR}", &dir.display().to_string());
    std::fs::write(dir.join(name), replaced).unwrap();
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
        write_file(
            dir,
            "system.toml",
            "[node]\nname = \"test\"\n\n[server]\nbind = \"0.0.0.0:443\"\ntls_cert = \"{DIR}/cert.pem\"\ntls_key = \"{DIR}/key.pem\"\n",
        );
    }

    #[test]
    fn validation_missing_site_name_exits_2() {
        let dir = tempfile::tempdir().unwrap();
        setup(
            dir.path(),
            r#"
[site]
domain = "example.com"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
"#,
        );
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("name"), "error should mention 'name': {}", err);
    }

    #[test]
    fn validation_missing_site_domain_exits_2() {
        let dir = tempfile::tempdir().unwrap();
        setup(
            dir.path(),
            r#"
[site]
name = "Test"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
"#,
        );
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("domain"),
            "error should mention 'domain': {}",
            err
        );
    }

    #[test]
    fn validation_missing_bind_exits_2() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "cert.pem", "dummy");
        write_file(dir.path(), "key.pem", "dummy");
        write_file(
            dir.path(),
            "site.toml",
            "[site]\nname = \"Test\"\ndomain = \"example.com\"\n\n[[backend]]\nname = \"b\"\nsockets = \"/run/m6/*.sock\"\n",
        );
        write_file(
            dir.path(),
            "system.toml",
            "[node]\nname = \"test\"\n\n[server]\ntls_cert = \"{DIR}/cert.pem\"\ntls_key = \"{DIR}/key.pem\"\n",
        );
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("bind"), "error should mention 'bind': {}", err);
    }

    #[test]
    fn validation_tls_cert_not_found_exits_2() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "key.pem", "dummy");
        write_file(
            dir.path(),
            "site.toml",
            "[site]\nname = \"Test\"\ndomain = \"example.com\"\n\n[[backend]]\nname = \"b\"\nsockets = \"/run/m6/*.sock\"\n",
        );
        write_file(
            dir.path(),
            "system.toml",
            "[node]\nname = \"test\"\n\n[server]\nbind = \"0.0.0.0:443\"\ntls_cert = \"{DIR}/missing.pem\"\ntls_key = \"{DIR}/key.pem\"\n",
        );
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("tls_cert"),
            "error should mention 'tls_cert': {}",
            err
        );
    }

    #[test]
    fn validation_unknown_backend_in_route_exits_2() {
        let dir = tempfile::tempdir().unwrap();
        setup(
            dir.path(),
            r#"
[site]
name   = "Test"
domain = "example.com"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
[[route]]
path    = "/"
backend = "unknown"
"#,
        );
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("unknown backend"),
            "error should mention backend: {}",
            err
        );
    }

    #[test]
    fn validation_duplicate_route_path_exits_2() {
        let dir = tempfile::tempdir().unwrap();
        setup(
            dir.path(),
            r#"
[site]
name   = "Test"
domain = "example.com"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
[[route]]
path    = "/"
backend = "b"
[[route]]
path    = "/"
backend = "b"
"#,
        );
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("duplicate"),
            "error should mention 'duplicate': {}",
            err
        );
    }

    #[test]
    fn validation_require_without_auth_exits_2() {
        let dir = tempfile::tempdir().unwrap();
        setup(
            dir.path(),
            r#"
[site]
name   = "Test"
domain = "example.com"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
[[route]]
path    = "/"
backend = "b"
require = "group:editors"
"#,
        );
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("require"),
            "error should mention 'require': {}",
            err
        );
    }

    #[test]
    fn validation_custom_errors_without_path_exits_2() {
        let dir = tempfile::tempdir().unwrap();
        setup(
            dir.path(),
            r#"
[site]
name   = "Test"
domain = "example.com"
[errors]
mode = "custom"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
"#,
        );
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("custom"),
            "error should mention 'custom': {}",
            err
        );
    }

    #[test]
    fn validation_auth_public_key_not_found_exits_2() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "cert.pem", "dummy");
        write_file(dir.path(), "key.pem", "dummy");
        write_file(
            dir.path(),
            "site.toml",
            "[site]\nname = \"Test\"\ndomain = \"example.com\"\n\n[[backend]]\nname = \"b\"\nsockets = \"/run/m6/*.sock\"\n",
        );
        write_file(
            dir.path(),
            "system.toml",
            "[node]\nname = \"test\"\n\n[server]\nbind = \"0.0.0.0:443\"\ntls_cert = \"{DIR}/cert.pem\"\ntls_key = \"{DIR}/key.pem\"\n\n[auth]\nbackend = \"b\"\npublic_key = \"{DIR}/missing-key.pub\"\n",
        );
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("public_key"),
            "error should mention 'public_key': {}",
            err
        );
    }

    #[test]
    fn validation_auth_backend_not_in_backends_exits_2() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "cert.pem", "dummy");
        write_file(dir.path(), "key.pem", "dummy");
        write_file(dir.path(), "auth.pub", "dummy-key");
        write_file(
            dir.path(),
            "site.toml",
            "[site]\nname = \"Test\"\ndomain = \"example.com\"\n\n[[backend]]\nname = \"b\"\nsockets = \"/run/m6/*.sock\"\n",
        );
        write_file(
            dir.path(),
            "system.toml",
            "[node]\nname = \"test\"\n\n[server]\nbind = \"0.0.0.0:443\"\ntls_cert = \"{DIR}/cert.pem\"\ntls_key = \"{DIR}/key.pem\"\n\n[auth]\nbackend = \"missing-auth-backend\"\npublic_key = \"{DIR}/auth.pub\"\n",
        );
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("missing-auth-backend") || err.contains("backend"),
            "error should mention auth backend: {}",
            err
        );
    }

    #[test]
    fn validation_auth_backend_is_a_url_backend_exits_2() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "cert.pem", "dummy");
        write_file(dir.path(), "key.pem", "dummy");
        write_file(dir.path(), "auth.pub", "dummy-key");
        write_file(
            dir.path(),
            "site.toml",
            "[site]\nname = \"Test\"\ndomain = \"example.com\"\n\n[[backend]]\nname = \"remote\"\nurl = \"https://example.com\"\n",
        );
        write_file(
            dir.path(),
            "system.toml",
            "[node]\nname = \"test\"\n\n[server]\nbind = \"0.0.0.0:443\"\ntls_cert = \"{DIR}/cert.pem\"\ntls_key = \"{DIR}/key.pem\"\n\n[auth]\nbackend = \"remote\"\npublic_key = \"{DIR}/auth.pub\"\n",
        );
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("URL backend"),
            "the error must say the auth backend is a URL backend: {err}"
        );
        assert!(
            err.contains("remote"),
            "the error must name the backend: {err}"
        );
    }

    #[test]
    fn bind_comes_from_the_system_config() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "cert.pem", "dummy");
        write_file(dir.path(), "key.pem", "dummy");
        write_file(
            dir.path(),
            "site.toml",
            "[site]\nname = \"Test\"\ndomain = \"example.com\"\n\n[[backend]]\nname = \"b\"\nsockets = \"/run/m6/*.sock\"\n",
        );
        write_file(
            dir.path(),
            "system.toml",
            "[node]\nname = \"test\"\n\n[server]\nbind = \"0.0.0.0:443\"\ntls_cert = \"{DIR}/cert.pem\"\ntls_key = \"{DIR}/key.pem\"\n",
        );
        let cfg = m6_http_lib::config::load(dir.path(), &dir.path().join("system.toml")).unwrap();
        assert_eq!(cfg.server.bind, "0.0.0.0:443");
    }
}

// ── Phase 11: Caching ────────────────────────────────────────────────────────

mod phase11 {
    use m6_http_lib::cache::{should_cache, Cache, CacheKey, CachedResponse};

    #[test]
    fn public_response_is_cached() {
        let cache = Cache::new();
        let key = CacheKey::new("/page", None, "");
        let headers = vec![(
            "cache-control".to_string(),
            "public, max-age=3600".to_string(),
        )];
        assert!(should_cache(200, &headers));

        let resp = CachedResponse {
            genid: 0,
            status: 200,
            headers: m6_http_lib::cache::HeaderBlock::from_pairs(headers.clone()),
            body: bytes::Bytes::from_static(b"cached"),
        };
        cache.insert(key.clone(), resp);
        assert!(cache.get(&key).is_some());
    }

    #[test]
    fn second_request_served_from_cache() {
        let cache = Cache::new();
        let key = CacheKey::new("/page", None, "");
        let resp = CachedResponse {
            genid: 0,
            status: 200,
            headers: m6_http_lib::cache::HeaderBlock::from_pairs(vec![(
                "cache-control".to_string(),
                "public".to_string(),
            )]),
            body: bytes::Bytes::from_static(b"body"),
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
        let key_gzip = CacheKey::new("/page", None, "gzip");
        let key_br = CacheKey::new("/page", None, "br");

        cache.insert(
            key_gzip.clone(),
            CachedResponse {
                genid: 0,
                status: 200,
                headers: m6_http_lib::cache::HeaderBlock::from_pairs(vec![(
                    "content-encoding".to_string(),
                    "gzip".to_string(),
                )]),
                body: bytes::Bytes::from_static(b"gzip-body"),
            },
        );

        assert!(cache.get(&key_gzip).is_some());
        assert!(cache.get(&key_br).is_none());

        cache.insert(
            key_br.clone(),
            CachedResponse {
                genid: 0,
                status: 200,
                headers: m6_http_lib::cache::HeaderBlock::from_pairs(vec![(
                    "content-encoding".to_string(),
                    "br".to_string(),
                )]),
                body: bytes::Bytes::from_static(b"br-body"),
            },
        );

        assert_eq!(cache.get(&key_gzip).unwrap().body, b"gzip-body" as &[u8]);
        assert_eq!(cache.get(&key_br).unwrap().body, b"br-body" as &[u8]);
    }

    /// The query is a *separate* key component. A `?` left inside the `path`
    /// argument is defensively stripped, so it can never smuggle one request's
    /// query into another request's key.
    #[test]
    fn query_string_is_part_of_cache_key() {
        // Distinct queries must not collide.
        let a = CacheKey::new("/blog", Some("a=1"), "");
        let b = CacheKey::new("/blog", Some("a=2"), "");
        let none = CacheKey::new("/blog", None, "");
        assert_ne!(a, b);
        assert_ne!(a, none);

        // A `?` in the path argument is ignored, not treated as a query.
        assert_eq!(CacheKey::new("/blog?a=1", None, ""), none);
    }

    #[test]
    fn data_file_modified_evicts_cache_entries() {
        let cache = Cache::new();

        // Pre-populate cache with the path that maps from a data file
        let key = CacheKey::new("/blog/hello-world", None, "");
        cache.insert(
            key.clone(),
            CachedResponse {
                genid: 0,
                status: 200,
                headers: m6_http_lib::cache::HeaderBlock::from_pairs(vec![(
                    "cache-control".to_string(),
                    "public".to_string(),
                )]),
                body: bytes::Bytes::from_static(b"hello world post"),
            },
        );

        // Simulate: content/posts/hello-world.json changed, and the invalidation
        // map maps that file to /blog/hello-world
        // evict the affected cache entry
        cache.evict_path("/blog/hello-world");
        assert!(cache.get(&key).is_none());
    }

    /// Was `error_responses_never_cached`, which lumped 404 in with 5xx.
    /// They are not the same thing: RFC 9111 3 lists 404 as heuristically
    /// cacheable, while 5xx are transient failures that must never be stored
    /// and replayed. Storing the 404 is what stops a cache node paying an
    /// origin round trip for every unknown path.
    #[test]
    fn server_errors_are_never_cached_but_a_404_may_be() {
        let public = vec![("cache-control".to_string(), "public".to_string())];
        assert!(should_cache(404, &public), "404 is storable per RFC 9111 3");
        for s in [500u16, 502, 503, 504] {
            assert!(
                !should_cache(s, &public),
                "{s} is a transient failure, never store it"
            );
        }
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
    fn extract_token_returns_none_without_an_auth_header_or_session_cookie() {
        let token = extract_token(None, None);
        assert_eq!(token, None);
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
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&encoded)
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(json["sub"], "user1");
        assert_eq!(json["groups"][0], "editors");
    }
}

// ── Config load ───────────────────────────────────────────────────────────────

mod config_load {
    #[test]
    fn a_changed_site_toml_loads_into_a_new_route_table() {
        use m6_http_lib::config;
        use m6_http_lib::router::RouteTable;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let dir_path = dir.path();

        // Write initial config
        std::fs::write(dir_path.join("cert.pem"), "dummy").unwrap();
        std::fs::write(dir_path.join("key.pem"), "dummy").unwrap();
        std::fs::write(
            dir_path.join("system.toml"),
            format!(
                "[node]\nname = \"test\"\n\n[server]\nbind = \"0.0.0.0:443\"\ntls_cert = \"{}/cert.pem\"\ntls_key = \"{}/key.pem\"\n",
                dir_path.display(),
                dir_path.display()
            ),
        )
        .unwrap();
        std::fs::write(
            dir_path.join("site.toml"),
            r#"
[site]
name   = "Test"
domain = "example.com"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
[[route]]
path    = "/"
backend = "b"
"#,
        )
        .unwrap();

        let cfg1 = config::load(dir_path, &dir_path.join("system.toml")).unwrap();
        let table1 = RouteTable::from_config(&cfg1).unwrap();
        assert!(table1.at("/").is_some());
        assert!(table1.at("/new").is_none());

        // Write updated config with new route
        std::fs::write(
            dir_path.join("site.toml"),
            r#"
[site]
name   = "Test"
domain = "example.com"
[[backend]]
name = "b"
sockets = "/run/m6/*.sock"
[[route]]
path    = "/"
backend = "b"
[[route]]
path    = "/new"
backend = "b"
"#,
        )
        .unwrap();

        let cfg2 = config::load(dir_path, &dir_path.join("system.toml")).unwrap();
        let table2 = RouteTable::from_config(&cfg2).unwrap();
        assert!(table2.at("/").is_some());
        assert!(table2.at("/new").is_some());
    }
}
